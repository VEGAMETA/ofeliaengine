// The C face of Luau's analysis for engine_luau_analysis (ADR 0107 §6): modules held in
// memory or read from the project's folder, checked by Luau's Frontend (type errors and
// lint warnings), the engine's type definitions loaded into its globals. Nothing throws
// across: Luau's own exceptions become one diagnostic.

#include "Luau/BuiltinDefinitions.h"
#include "Luau/Error.h"
#include "Luau/FileResolver.h"
#include "Luau/Frontend.h"
#include "Luau/Linter.h"
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
    std::vector<std::string> messages;
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
        Luau::FrontendOptions options;
        options.retainFullTypeGraphs = true;
        options.runLintChecks = true;
        a->frontend = std::make_unique<Luau::Frontend>(
            new_solver ? Luau::SolverMode::New : Luau::SolverMode::Old, &a->files, &a->configs, options
        );
        Luau::registerBuiltinGlobals(*a->frontend, a->frontend->globals);
        Luau::freeze(a->frontend->globals.globalTypes);
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

} // extern "C"
