//! `#[derive(Reflect)]` for `engine_reflect`: generates a static `TypeInfo` with field
//! offsets and attributes for structs (named or tuple) and C-like enums with a `repr`.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as Tokens;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Expr, Fields, LitStr, parse_macro_input};

/// Derives `engine_reflect::Reflect`.
///
/// Attributes on fields, all optional: `#[reflect(min = 0.0, max = 1.0, step = 0.1,
/// tooltip = "...", hidden, skip, net_quantum = 0.01, net_skip, hint = "color",
/// rename = "...")]`. On the type: `#[reflect(name = "...", skip, version = 2)]` (`skip`:
/// its values are not saved; `version`: the version saves record, ADR 0104).
#[proc_macro_derive(Reflect, attributes(reflect))]
pub fn derive_reflect(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

#[derive(Default)]
struct Attrs {
    min: Option<Expr>,
    max: Option<Expr>,
    step: Option<Expr>,
    tooltip: Option<LitStr>,
    hidden: bool,
    skip: bool,
    net_quantum: Option<Expr>,
    net_skip: bool,
    hint: Option<LitStr>,
    rename: Option<LitStr>,
    version: Option<syn::LitInt>,
}

fn parse_attrs(attrs: &[syn::Attribute]) -> syn::Result<Attrs> {
    let mut out = Attrs::default();
    for attr in attrs {
        if !attr.path().is_ident("reflect") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map(ToString::to_string)
                .unwrap_or_default();
            match key.as_str() {
                "min" => out.min = Some(meta.value()?.parse()?),
                "max" => out.max = Some(meta.value()?.parse()?),
                "step" => out.step = Some(meta.value()?.parse()?),
                "net_quantum" => out.net_quantum = Some(meta.value()?.parse()?),
                "tooltip" => out.tooltip = Some(meta.value()?.parse()?),
                "hint" => out.hint = Some(meta.value()?.parse()?),
                "rename" | "name" => out.rename = Some(meta.value()?.parse()?),
                "version" => out.version = Some(meta.value()?.parse()?),
                "hidden" => out.hidden = true,
                "skip" => out.skip = true,
                "net_skip" => out.net_skip = true,
                _ => return Err(meta.error("unknown reflect attribute")),
            }
            Ok(())
        })?;
    }
    Ok(out)
}

fn attrs_tokens(a: &Attrs) -> syn::Result<Tokens> {
    let opt = |e: &Option<Expr>| match e {
        Some(e) => quote!(::core::option::Option::Some((#e) as f64)),
        None => quote!(::core::option::Option::None),
    };
    let (min, max, step, quantum) = (opt(&a.min), opt(&a.max), opt(&a.step), opt(&a.net_quantum));
    let tooltip = match &a.tooltip {
        Some(t) => quote!(::core::option::Option::Some(#t)),
        None => quote!(::core::option::Option::None),
    };
    let hint = match a.hint.as_ref().map(LitStr::value).as_deref() {
        None => quote!(::engine_reflect::Hint::None),
        Some("color") => quote!(::engine_reflect::Hint::Color),
        Some("degrees") => quote!(::engine_reflect::Hint::Degrees),
        Some("multiline") => quote!(::engine_reflect::Hint::Multiline),
        Some(_) => {
            return Err(syn::Error::new_spanned(
                a.hint.as_ref(),
                "hint must be \"color\", \"degrees\" or \"multiline\"",
            ));
        }
    };
    let (hidden, skip, net_skip) = (a.hidden, a.skip, a.net_skip);
    let version = match &a.version {
        Some(v) => {
            let v: u32 = v.base10_parse()?;
            quote!(#v)
        }
        None => quote!(0),
    };
    Ok(quote! {
        ::engine_reflect::Attrs {
            min: #min,
            max: #max,
            step: #step,
            tooltip: #tooltip,
            hidden: #hidden,
            skip: #skip,
            net_quantum: #quantum,
            net_skip: #net_skip,
            hint: #hint,
            version: #version,
        }
    })
}

fn expand(input: &DeriveInput) -> syn::Result<Tokens> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "Reflect cannot be derived for generic types yet",
        ));
    }
    let ident = &input.ident;
    let type_attrs = parse_attrs(&input.attrs)?;
    let name = type_attrs
        .rename
        .as_ref()
        .map_or_else(|| ident.to_string(), LitStr::value);
    let type_attr_tokens = attrs_tokens(&type_attrs)?;
    let kind = match &input.data {
        Data::Struct(data) => {
            let mut fields = Vec::new();
            let members: Vec<(Tokens, String, &syn::Field)> = match &data.fields {
                Fields::Named(named) => named
                    .named
                    .iter()
                    .filter_map(|f| f.ident.as_ref().map(|i| (quote!(#i), i.to_string(), f)))
                    .collect(),
                Fields::Unnamed(unnamed) => unnamed
                    .unnamed
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let index = syn::Index::from(i);
                        (quote!(#index), i.to_string(), f)
                    })
                    .collect(),
                Fields::Unit => Vec::new(),
            };
            for (member, default_name, field) in members {
                let a = parse_attrs(&field.attrs)?;
                let field_name = a.rename.as_ref().map_or(default_name, LitStr::value);
                let ty = &field.ty;
                let attrs = attrs_tokens(&a)?;
                fields.push(quote! {
                    ::engine_reflect::Field {
                        name: #field_name,
                        offset: ::core::mem::offset_of!(#ident, #member),
                        ty: <#ty as ::engine_reflect::Reflect>::TYPE_INFO,
                        attrs: #attrs,
                    }
                });
            }
            quote!(::engine_reflect::TypeKind::Struct(&[#(#fields),*]))
        }
        Data::Enum(data) => {
            let repr = input
                .attrs
                .iter()
                .filter(|a| a.path().is_ident("repr"))
                .find_map(|a| {
                    let mut found = None;
                    let _ = a.parse_nested_meta(|meta| {
                        if let Some(i) = meta.path.get_ident() {
                            let s = i.to_string();
                            if matches!(
                                s.as_str(),
                                "u8" | "u16" | "u32" | "u64" | "i8" | "i16" | "i32" | "i64"
                            ) {
                                found = Some(s);
                            }
                        }
                        Ok(())
                    });
                    found
                })
                .ok_or_else(|| {
                    syn::Error::new_spanned(
                        ident,
                        "Reflect on an enum needs #[repr(u8)] (or another integer)",
                    )
                })?;
            let prim = format_ident!("{}", repr.to_uppercase());
            let mut variants = Vec::new();
            for v in &data.variants {
                if !matches!(v.fields, Fields::Unit) {
                    return Err(syn::Error::new_spanned(
                        v,
                        "Reflect supports C-like enums only",
                    ));
                }
                let vi = &v.ident;
                let a = parse_attrs(&v.attrs)?;
                let vname = a
                    .rename
                    .as_ref()
                    .map_or_else(|| vi.to_string(), LitStr::value);
                variants.push(quote! {
                    ::engine_reflect::Variant { name: #vname, value: #ident::#vi as i64 }
                });
            }
            quote! {
                ::engine_reflect::TypeKind::Enum {
                    repr: ::engine_reflect::Primitive::#prim,
                    variants: &[#(#variants),*],
                }
            }
        }
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                ident,
                "Reflect cannot be derived for unions",
            ));
        }
    };
    Ok(quote! {
        impl ::engine_reflect::Reflect for #ident {
            const TYPE_INFO: &'static ::engine_reflect::TypeInfo = &::engine_reflect::TypeInfo {
                name: #name,
                size: ::core::mem::size_of::<#ident>(),
                align: ::core::mem::align_of::<#ident>(),
                kind: #kind,
                attrs: #type_attr_tokens,
            };
        }
    })
}
