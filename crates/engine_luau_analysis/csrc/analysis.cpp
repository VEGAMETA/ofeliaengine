// The C face of Luau's analysis for engine_luau_analysis (ADR 0107 §6): modules held in
// memory or read from the project's folder, checked by Luau's Frontend (type errors and
// lint warnings), the engine's type definitions loaded into its globals. Nothing throws
// across: Luau's own exceptions become one diagnostic.

#include "Luau/AstQuery.h"
#include "Luau/Autocomplete.h"
#include "Luau/BuiltinDefinitions.h"
#include "Luau/Error.h"
#include "Luau/FileResolver.h"
#include "Luau/Frontend.h"
#include "Luau/Linter.h"
#include "Luau/ToString.h"
#include "Luau/TypePack.h"
#include "Luau/TypeArena.h"

#include <cstdint>
#include <cstring>
#include <fstream>
#include <sstream>
#include <string>
#include <unordered_map>
#include <vector>

namespace
{

// A module's name is its path from the project's folder, with `/`.
struct ProjectFiles : Luau::FileResolver
{
    std::string root;
    std::unordered_map<std::string, std::string> open;

    std::optional<std::string> text(const std::string& name) const
    {
        if (auto it = open.find(name); it != open.end())
            return it->second;
        std::ifstream file(root + "/" + name, std::ios::binary);
        if (!file)
            return std::nullopt;
        std::ostringstream out;
        out << file.rdbuf();
        return out.str();
    }

    std::optional<Luau::SourceCode> readSource(const Luau::ModuleName& name) override
    {
        auto source = text(name);
        if (!source)
            return std::nullopt;
        return Luau::SourceCode{*source, Luau::SourceCode::Module};
    }

    // `require("./door")` and `require("../shared/math")`: by string, from the requiring
    // module's folder, `name.luau` or `name/init.luau` (Luau's require by string). Aliases
    // (`@game/…` from `.luaurc`) come with the engine's scripts (ADR 0114).
    std::optional<Luau::ModuleInfo> resolveModule(
        const Luau::ModuleInfo* context,
        Luau::AstExpr* expr,
        const Luau::TypeCheckLimits&
    ) override
    {
        auto* string = expr ? expr->as<Luau::AstExprConstantString>() : nullptr;
        if (!string || !context)
            return std::nullopt;
        std::string path(string->value.data, string->value.size);
        if (path.rfind("./", 0) != 0 && path.rfind("../", 0) != 0)
            return std::nullopt;
        std::string from = context->name;
        size_t slash = from.find_last_of('/');
        std::string joined = slash == std::string::npos ? path : from.substr(0, slash + 1) + path;
        std::string name = normalize(joined);
        if (name.empty())
            return std::nullopt;
        for (const std::string& candidate : {name + ".luau", name + "/init.luau"})
        {
            if (open.count(candidate) || std::ifstream(root + "/" + candidate, std::ios::binary))
                return Luau::ModuleInfo{candidate};
        }
        return std::nullopt;
    }

    std::string getHumanReadableModuleName(const Luau::ModuleName& name) const override
    {
        return name;
    }

    // `a/./b/../c` → `a/c`; empty when it climbs out of the project.
    static std::string normalize(const std::string& path)
    {
        std::vector<std::string> parts;
        size_t start = 0;
        while (start <= path.size())
        {
            size_t end = path.find('/', start);
            if (end == std::string::npos)
                end = path.size();
            std::string part = path.substr(start, end - start);
            if (part == "..")
            {
                if (parts.empty())
                    return {};
                parts.pop_back();
            }
            else if (!part.empty() && part != ".")
                parts.push_back(part);
            start = end + 1;
        }
        std::string out;
        for (const std::string& part : parts)
        {
            if (!out.empty())
                out += '/';
            out += part;
        }
        return out;
    }
};

} // namespace

extern "C"
{

struct EngineAnalysis
{
    ProjectFiles files;
    Luau::NullConfigResolver configs;
    std::unique_ptr<Luau::Frontend> frontend;
    bool new_solver = true;
    std::vector<std::string> messages;
};

struct EngineCompletion
{
    const char* label;
    size_t label_len;
    // Luau's AutocompleteEntryKind.
    uint32_t kind;
    // 1 deprecated, 2 of the type wanted there, 4 a function (parentheses after it), 8
    // one that takes no arguments (the caret after them).
    uint32_t flags;
    // Its type, as Luau writes it.
    const char* detail;
    size_t detail_len;
};

struct EngineDiagnostic
{
    // From 0; columns in bytes.
    uint32_t line;
    uint32_t column;
    uint32_t end_line;
    uint32_t end_column;
    // 0 an error, 1 a warning.
    uint32_t severity;
    const char* message;
    size_t message_len;
};

EngineAnalysis* engine_analysis_new(const char* root, size_t root_len, int new_solver)
{
    try
    {
        auto* a = new EngineAnalysis();
        a->files.root.assign(root, root_len);
        a->new_solver = new_solver != 0;
        Luau::FrontendOptions options;
        options.retainFullTypeGraphs = true;
        options.runLintChecks = true;
        a->frontend = std::make_unique<Luau::Frontend>(
            new_solver ? Luau::SolverMode::New : Luau::SolverMode::Old, &a->files, &a->configs, options
        );
        Luau::registerBuiltinGlobals(*a->frontend, a->frontend->globals);
        Luau::freeze(a->frontend->globals.globalTypes);
        // The old solver completes from globals of its own.
        if (!new_solver)
        {
            Luau::registerBuiltinGlobals(*a->frontend, a->frontend->globalsForAutocomplete, true);
            Luau::freeze(a->frontend->globalsForAutocomplete.globalTypes);
        }
        return a;
    }
    catch (...)
    {
        return nullptr;
    }
}

void engine_analysis_free(EngineAnalysis* a)
{
    delete a;
}

void engine_analysis_set_source(EngineAnalysis* a, const char* name, size_t name_len, const char* text, size_t text_len)
{
    std::string module(name, name_len);
    a->files.open[module].assign(text, text_len);
    a->frontend->markDirty(module);
}

void engine_analysis_forget(EngineAnalysis* a, const char* name, size_t name_len)
{
    std::string module(name, name_len);
    a->files.open.erase(module);
    a->frontend->markDirty(module);
}

// Loads type definitions (a `.d.luau`) into the globals every module sees. Returns 0, or
// the number of problems in them, the first one's text in `error` (valid until the next
// call).
size_t engine_analysis_add_definitions(
    EngineAnalysis* a,
    const char* name,
    size_t name_len,
    const char* text,
    size_t text_len,
    const char** error,
    size_t* error_len
)
{
    a->messages.clear();
    size_t problems = 0;
    try
    {
        Luau::GlobalTypes& globals = a->frontend->globals;
        Luau::unfreeze(globals.globalTypes);
        Luau::LoadDefinitionFileResult result = a->frontend->loadDefinitionFile(
            globals, globals.globalScope, std::string_view(text, text_len), std::string(name, name_len), false
        );
        Luau::freeze(globals.globalTypes);
        if (!a->new_solver)
        {
            Luau::GlobalTypes& completing = a->frontend->globalsForAutocomplete;
            Luau::unfreeze(completing.globalTypes);
            a->frontend->loadDefinitionFile(
                completing, completing.globalScope, std::string_view(text, text_len), std::string(name, name_len), false, true
            );
            Luau::freeze(completing.globalTypes);
        }
        for (const Luau::ParseError& e : result.parseResult.errors)
        {
            if (a->messages.empty())
                a->messages.push_back(std::to_string(e.getLocation().begin.line + 1) + ": " + e.getMessage());
            problems++;
        }
        if (result.module)
        {
            for (const Luau::TypeError& e : result.module->errors)
            {
                if (a->messages.empty())
                    a->messages.push_back(std::to_string(e.location.begin.line + 1) + ": " + Luau::toString(e));
                problems++;
            }
        }
        if (!result.success && problems == 0)
        {
            a->messages.push_back("the definitions were not loaded");
            problems = 1;
        }
    }
    catch (const std::exception& e)
    {
        a->messages.push_back(std::string("the analysis failed: ") + e.what());
        problems = 1;
    }
    catch (...)
    {
        a->messages.push_back("the analysis failed");
        problems = 1;
    }
    if (!a->messages.empty())
    {
        *error = a->messages[0].data();
        *error_len = a->messages[0].size();
    }
    return problems;
}

// Checks module `name` (and what it requires): its type errors and lint warnings into
// `out`, at most `cap`. Returns how many there are in all; the messages stay valid until
// the next call.
size_t engine_analysis_check(EngineAnalysis* a, const char* name, size_t name_len, EngineDiagnostic* out, size_t cap)
{
    a->messages.clear();
    std::string module(name, name_len);
    struct Found
    {
        Luau::Location location;
        uint32_t severity;
    };
    std::vector<Found> found;
    try
    {
        Luau::CheckResult result = a->frontend->check(module);
        for (const Luau::TypeError& e : result.errors)
        {
            // What other modules have wrong is theirs to show.
            if (e.moduleName != module)
                continue;
            found.push_back({e.location, 0});
            a->messages.push_back(Luau::toString(e));
        }
        for (const Luau::LintWarning& w : result.lintResult.errors)
        {
            found.push_back({w.location, 0});
            a->messages.push_back(std::string(Luau::LintWarning::getName(w.code)) + ": " + w.text);
        }
        for (const Luau::LintWarning& w : result.lintResult.warnings)
        {
            found.push_back({w.location, 1});
            a->messages.push_back(std::string(Luau::LintWarning::getName(w.code)) + ": " + w.text);
        }
    }
    catch (const std::exception& e)
    {
        found.push_back({Luau::Location(), 0});
        a->messages.push_back(std::string("the analysis failed: ") + e.what());
    }
    catch (...)
    {
        found.push_back({Luau::Location(), 0});
        a->messages.push_back("the analysis failed");
    }
    size_t n = found.size() < cap ? found.size() : cap;
    for (size_t i = 0; i < n; ++i)
    {
        const Luau::Location& l = found[i].location;
        out[i] = EngineDiagnostic{
            l.begin.line,
            l.begin.column,
            l.end.line,
            l.end.column,
            found[i].severity,
            a->messages[i].data(),
            a->messages[i].size(),
        };
    }
    return found.size();
}

// What may be written at `line` and `column` (from 0, a byte) of module `name`, checked
// first where it changed: into `out`, at most `cap`. Returns how many there are; their
// texts stay valid until the next call.
size_t engine_analysis_complete(
    EngineAnalysis* a,
    const char* name,
    size_t name_len,
    uint32_t line,
    uint32_t column,
    EngineCompletion* out,
    size_t cap
)
{
    a->messages.clear();
    std::string module(name, name_len);
    struct Found
    {
        uint32_t kind;
        uint32_t flags;
    };
    std::vector<Found> found;
    try
    {
        if (a->new_solver)
            a->frontend->check(module);
        else
        {
            Luau::FrontendOptions options;
            options.retainFullTypeGraphs = true;
            options.forAutocomplete = true;
            a->frontend->check(module, options);
        }
        // Strings for tagged functions (a `.d.luau`'s `@tag`): none yet.
        auto strings = [](std::string, std::optional<const Luau::ExternType*>, std::optional<std::string>
                       ) -> std::optional<Luau::AutocompleteEntryMap>
        {
            return std::nullopt;
        };
        Luau::AutocompleteResult result = Luau::autocomplete(*a->frontend, module, Luau::Position{line, column}, strings);
        Luau::ToStringOptions shown;
        shown.maxTypeLength = 160;
        shown.hideFunctionSelfArgument = true;
        for (const auto& [label, entry] : result.entryMap)
        {
            if (label == Luau::kGeneratedAnonymousFunctionEntryName)
                continue;
            uint32_t flags = 0;
            if (entry.deprecated)
                flags |= 1;
            if (entry.typeCorrect != Luau::TypeCorrectKind::None)
                flags |= 2;
            if (entry.parens != Luau::ParenthesesRecommendation::None)
                flags |= 4;
            if (entry.parens == Luau::ParenthesesRecommendation::CursorAfter)
                flags |= 8;
            found.push_back({uint32_t(entry.kind), flags});
            a->messages.push_back(label);
            a->messages.push_back(entry.type ? Luau::toString(*entry.type, shown) : std::string());
        }
    }
    catch (...)
    {
        found.clear();
        a->messages.clear();
    }
    size_t n = found.size() < cap ? found.size() : cap;
    for (size_t i = 0; i < n; ++i)
    {
        const std::string& label = a->messages[2 * i];
        const std::string& detail = a->messages[2 * i + 1];
        out[i] = EngineCompletion{label.data(), label.size(), found[i].kind, found[i].flags, detail.data(), detail.size()};
    }
    return found.size();
}

// What is at `line` and `column` of module `name` (as last checked): the name's or the
// expression's type, as Luau writes it, into `text` (valid until the next call); 0 when
// nothing is there.
size_t engine_analysis_hover(
    EngineAnalysis* a,
    const char* name,
    size_t name_len,
    uint32_t line,
    uint32_t column,
    const char** text
)
{
    a->messages.clear();
    std::string module(name, name_len);
    try
    {
        a->frontend->check(module);
        const Luau::SourceModule* source = a->frontend->getSourceModule(module);
        Luau::ModulePtr checked = a->frontend->moduleResolver.getModule(module);
        if (!source || !checked)
            return 0;
        Luau::Position at{line, column};
        Luau::ToStringOptions shown;
        shown.maxTypeLength = 400;
        shown.useLineBreaks = false;
        // A table's fields, not the name the solver gave it after its local.
        shown.ignoreSyntheticName = true;
        std::optional<Luau::TypeId> type;
        // A name where it is declared (`local door`, a parameter): its own binding, which
        // findBindingAtPosition leaves out inside its definition.
        if (Luau::AstLocal* local = Luau::findExprOrLocalAtPosition(*source, at).getLocal())
        {
            for (Luau::ScopePtr scope = Luau::findScopeAtPosition(*checked, at); scope && !type; scope = scope->parent)
            {
                auto it = scope->bindings.find(Luau::Symbol(local));
                if (it != scope->bindings.end())
                    type = it->second.typeId;
            }
        }
        if (!type)
        {
            if (std::optional<Luau::Binding> binding = Luau::findBindingAtPosition(*checked, *source, at))
                type = binding->typeId;
            else
                type = Luau::findTypeAtPosition(*checked, *source, at);
        }
        if (!type)
            return 0;
        a->messages.push_back(Luau::toString(*type, shown));
    }
    catch (...)
    {
        return 0;
    }
    *text = a->messages[0].data();
    return a->messages[0].size();
}

// Where what is at `line` and `column` of module `name` is defined: its module (into
// `module`, valid until the next call) and its place. Returns 1 when found: a local's or a
// global's binding, a field's place in its table, a function where it was written.
int engine_analysis_definition(
    EngineAnalysis* a,
    const char* name,
    size_t name_len,
    uint32_t line,
    uint32_t column,
    const char** module_out,
    size_t* module_len,
    uint32_t* out_line,
    uint32_t* out_column
)
{
    a->messages.clear();
    std::string module(name, name_len);
    try
    {
        a->frontend->check(module);
        const Luau::SourceModule* source = a->frontend->getSourceModule(module);
        Luau::ModulePtr checked = a->frontend->moduleResolver.getModule(module);
        if (!source || !checked)
            return 0;
        Luau::Position at{line, column};
        std::optional<std::pair<std::string, Luau::Location>> found;
        // A built-in's place is nowhere in the project.
        auto placed = [](const Luau::Location& l)
        {
            return !(l.begin.line == 0 && l.begin.column == 0 && l.end.line == 0 && l.end.column == 0);
        };
        if (std::optional<Luau::Binding> binding = Luau::findBindingAtPosition(*checked, *source, at);
            binding && placed(binding->location))
            found = {module, binding->location};
        Luau::AstExpr* expr = found ? nullptr : Luau::findExprAtPosition(*source, at);
        if (auto* index = expr ? expr->as<Luau::AstExprIndexName>() : nullptr)
        {
            if (const Luau::TypeId* object = checked->astTypes.find(index->expr))
            {
                if (const Luau::TableType* table = Luau::getTableType(Luau::follow(*object)))
                {
                    auto it = table->props.find(std::string(index->index.value));
                    if (it != table->props.end() && it->second.location && placed(*it->second.location))
                        found = {table->definitionModuleName.empty() ? module : table->definitionModuleName, *it->second.location};
                }
            }
        }
        if (!found && expr)
        {
            if (const Luau::TypeId* type = checked->astTypes.find(expr))
            {
                const Luau::FunctionType* function = Luau::get<Luau::FunctionType>(Luau::follow(*type));
                if (function && function->definition && function->definition->definitionModuleName &&
                    placed(function->definition->originalNameLocation))
                    found = {*function->definition->definitionModuleName, function->definition->originalNameLocation};
            }
        }
        // Luau's own definitions (`@luau`) are not the project's.
        if (!found || found->first.rfind("@", 0) == 0)
            return 0;
        a->messages.push_back(found->first);
        *module_out = a->messages[0].data();
        *module_len = a->messages[0].size();
        *out_line = found->second.begin.line;
        *out_column = found->second.begin.column;
        return 1;
    }
    catch (...)
    {
        return 0;
    }
}

// The signature of the call whose parentheses `line` and `column` of module `name` are
// in: `name(a: number, b: string): boolean` into `text` (valid until the next call), and
// the byte range of the argument the place is at. Returns 1 when there is one.
int engine_analysis_signature(
    EngineAnalysis* a,
    const char* name,
    size_t name_len,
    uint32_t line,
    uint32_t column,
    const char** text_out,
    size_t* text_len,
    uint32_t* active_start,
    uint32_t* active_end
)
{
    a->messages.clear();
    std::string module(name, name_len);
    try
    {
        a->frontend->check(module);
        const Luau::SourceModule* source = a->frontend->getSourceModule(module);
        Luau::ModulePtr checked = a->frontend->moduleResolver.getModule(module);
        if (!source || !checked)
            return 0;
        Luau::Position at{line, column};
        // The innermost call whose parentheses hold the place.
        Luau::AstExprCall* call = nullptr;
        std::vector<Luau::AstNode*> ancestry = Luau::findAstAncestryOfPosition(*source, at);
        for (auto it = ancestry.rbegin(); it != ancestry.rend() && !call; ++it)
        {
            auto* c = (*it)->as<Luau::AstExprCall>();
            // The arguments' place starts after `(` and ends at `)`.
            if (c && c->argLocation.begin <= at && at <= c->argLocation.end)
                call = c;
        }
        if (!call)
            return 0;
        const Luau::TypeId* type = checked->astTypes.find(call->func);
        const Luau::FunctionType* function = type ? Luau::get<Luau::FunctionType>(Luau::follow(*type)) : nullptr;
        if (!function)
            return 0;
        // The argument the place is at: how many end before it.
        size_t active = 0;
        for (Luau::AstExpr* arg : call->args)
            if (arg->location.end < at)
                active++;
        std::string callee = "function";
        if (auto* g = call->func->as<Luau::AstExprGlobal>())
            callee = g->name.value;
        else if (auto* l = call->func->as<Luau::AstExprLocal>())
            callee = l->local->name.value;
        else if (auto* i = call->func->as<Luau::AstExprIndexName>())
            callee = i->index.value;
        Luau::ToStringOptions shown;
        shown.maxTypeLength = 120;
        auto [params, tail] = Luau::flatten(function->argTypes);
        // A method called with `:` does not show its `self`.
        size_t first = call->self && function->hasSelf && !params.empty() ? 1 : 0;
        std::string text = callee + "(";
        uint32_t start = 0, end = 0;
        size_t shown_args = 0;
        for (size_t i = first; i < params.size(); ++i)
        {
            if (shown_args > 0)
                text += ", ";
            if (shown_args == active)
                start = uint32_t(text.size());
            if (i < function->argNames.size() && function->argNames[i] && !function->argNames[i]->name.empty())
                text += function->argNames[i]->name + ": ";
            text += Luau::toString(params[i], shown);
            if (shown_args == active)
                end = uint32_t(text.size());
            shown_args++;
        }
        // A variadic tail (`...number`), but the `...any` that non-strict code infers for
        // every function.
        std::string rest = tail ? Luau::toString(*tail, shown) : std::string();
        if (!rest.empty() && rest != "...any")
        {
            if (shown_args > 0)
                text += ", ";
            if (active >= shown_args)
                start = uint32_t(text.size());
            text += rest;
            if (active >= shown_args)
                end = uint32_t(text.size());
        }
        text += "): " + Luau::toString(function->retTypes, shown);
        a->messages.push_back(text);
        *text_out = a->messages[0].data();
        *text_len = a->messages[0].size();
        *active_start = start;
        *active_end = end;
        return 1;
    }
    catch (...)
    {
        return 0;
    }
}

} // extern "C"
