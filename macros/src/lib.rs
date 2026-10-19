use proc_macro::TokenStream;
use quote::quote;
use syn::{Item, LitStr, parse_macro_input};

/// Declares a saved resource or component. Registration and resource lifecycles
/// belong to SavePlugin, not to generated application systems.
#[proc_macro_attribute]
pub fn save(args: TokenStream, input: TokenStream) -> TokenStream {
    let mut role = None;
    let mut key = None;
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("resource") || meta.path.is_ident("component") {
            if role.is_some() {
                return Err(meta.error("choose resource or component once"));
            }
            role = Some(meta.path.clone());
            Ok(())
        } else if meta.path.is_ident("key") {
            if key.is_some() {
                return Err(meta.error("duplicate save key"));
            }
            let value: LitStr = meta.value()?.parse()?;
            if value.value().is_empty() {
                return Err(meta.error("save keys cannot be empty"));
            }
            key = Some(value);
            Ok(())
        } else {
            Err(meta.error("expected resource, component, or key = \"stable-key\""))
        }
    });
    parse_macro_input!(args with parser);
    let item = parse_macro_input!(input as Item);
    let (name, generics) = match &item {
        Item::Struct(item) => (&item.ident, &item.generics),
        Item::Enum(item) => (&item.ident, &item.generics),
        _ => {
            return syn::Error::new_spanned(item, "save requires a struct or enum")
                .into_compile_error()
                .into();
        }
    };
    if !generics.params.is_empty() {
        return syn::Error::new_spanned(generics, "automatic save declarations require a concrete type; generic integrations register concrete types in their plugin").into_compile_error().into();
    }
    let Some(role) = role else {
        return syn::Error::new_spanned(name, "specify resource or component")
            .into_compile_error()
            .into();
    };
    let Some(key) = key else {
        return syn::Error::new_spanned(name, "specify a stable save key")
            .into_compile_error()
            .into();
    };
    let root = match proc_macro_crate::crate_name("bevy_gigasaver") {
        Ok(proc_macro_crate::FoundCrate::Itself) => quote!(::bevy_gigasaver),
        Ok(proc_macro_crate::FoundCrate::Name(name)) => {
            let name = syn::Ident::new(&name, proc_macro2::Span::call_site());
            quote!(::#name)
        }
        Err(error) => {
            return syn::Error::new_spanned(name, error)
                .into_compile_error()
                .into();
        }
    };
    let registration = if role.is_ident("resource") {
        quote!(#root::__private::declare_resource::<#name>(app, #key);)
    } else {
        quote!(#root::__private::declare_component::<#name>(app, #key);)
    };
    quote! {
        #item
        #root::__private::inventory::submit! {
            #root::__private::Declaration {
                key: #key,
                install: |app| { #registration },
            }
        }
    }
    .into()
}
