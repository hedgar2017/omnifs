//! Attribute macro generating the complete `Object` impl for an object struct.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, ItemStruct, LitStr, Path, Token};

pub(crate) struct ObjectArgs {
    pub kind: LitStr,
    pub key: Path,
    pub canonical: Option<Ident>,
    pub decode: Option<Path>,
    pub load: Option<Path>,
    /// The object has no canonical bytes and no load.
    pub no_load: bool,
}

impl Parse for ObjectArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut kind: Option<LitStr> = None;
        let mut object_key: Option<Path> = None;
        let mut canonical: Option<Ident> = None;
        let mut decode: Option<Path> = None;
        let mut load: Option<Path> = None;
        let mut no_load: Option<Ident> = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            match key.to_string().as_str() {
                "no_load" => {
                    if no_load.is_some() {
                        return Err(syn::Error::new(key.span(), "duplicate `no_load` argument"));
                    }
                    no_load = Some(key);
                },
                "kind" => {
                    if kind.is_some() {
                        return Err(syn::Error::new(key.span(), "duplicate `kind` argument"));
                    }
                    let _: Token![=] = input.parse()?;
                    kind = Some(input.parse()?);
                },
                "key" => {
                    if object_key.is_some() {
                        return Err(syn::Error::new(key.span(), "duplicate `key` argument"));
                    }
                    let _: Token![=] = input.parse()?;
                    object_key = Some(input.parse()?);
                },
                "canonical" => {
                    if canonical.is_some() {
                        return Err(syn::Error::new(
                            key.span(),
                            "duplicate `canonical` argument",
                        ));
                    }
                    let _: Token![=] = input.parse()?;
                    canonical = Some(input.parse()?);
                },
                "decode" => {
                    if decode.is_some() {
                        return Err(syn::Error::new(key.span(), "duplicate `decode` argument"));
                    }
                    let _: Token![=] = input.parse()?;
                    decode = Some(input.parse()?);
                },
                "load" => {
                    if load.is_some() {
                        return Err(syn::Error::new(key.span(), "duplicate `load` argument"));
                    }
                    let _: Token![=] = input.parse()?;
                    load = Some(input.parse()?);
                },
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "supported object arguments are `kind = \"...\"`, `key = Type`, `canonical = Ident`, `decode = path::to::function`, `load = path::to::function`, and `no_load`",
                    ));
                },
            }
            if input.peek(Token![,]) {
                let _: Token![,] = input.parse()?;
            }
        }

        let kind = kind.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "missing required `kind = \"...\"` argument",
            )
        })?;
        let object_key = object_key.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "missing required `key = Type` argument",
            )
        })?;

        if let Some(no_load) = &no_load
            && (canonical.is_some() || decode.is_some() || load.is_some())
        {
            return Err(syn::Error::new(
                no_load.span(),
                "an object with `no_load` has no canonical bytes: it takes no `canonical`, `decode`, or `load` argument",
            ));
        }

        Ok(Self {
            kind,
            key: object_key,
            canonical,
            decode,
            load,
            no_load: no_load.is_some(),
        })
    }
}

pub(crate) fn object_item_impl(args: &ObjectArgs, item: &ItemStruct) -> syn::Result<TokenStream2> {
    let struct_ident = &item.ident;
    let (impl_generics, ty_generics, where_clause) = item.generics.split_for_impl();
    let kind_val = &args.kind;
    let object_key = &args.key;

    if args.no_load {
        return Ok(quote! {
            #item

            impl #impl_generics omnifs_sdk::object::Object for #struct_ident #ty_generics #where_clause {
                type Key = #object_key;
                type State = crate::__OmnifsProviderState;
                type Canonical = omnifs_sdk::repr::Json;

                const LOADS: bool = false;

                fn load(
                    _cx: &omnifs_sdk::Cx<Self::State>,
                    _key: &Self::Key,
                    _since: ::core::option::Option<omnifs_sdk::object::Validator>,
                ) -> impl ::core::future::Future<
                    Output = omnifs_sdk::error::Result<omnifs_sdk::object::Load<Self>>,
                > {
                    ::core::future::ready(Err(omnifs_sdk::object::no_canonical::<Self>()))
                }

                fn decode(_bytes: &[u8]) -> omnifs_sdk::error::Result<Self> {
                    Err(omnifs_sdk::object::no_canonical::<Self>())
                }

                fn kind() -> omnifs_sdk::object::ObjectKind {
                    omnifs_sdk::object::ObjectKind(#kind_val)
                }
            }
        });
    }

    let canonical_ident = args
        .canonical
        .clone()
        .unwrap_or_else(|| Ident::new("Json", proc_macro2::Span::call_site()));
    let is_json = canonical_ident == "Json";

    // `decode` defaults to JSON decode for `Json` canonical (the type must be
    // `DeserializeOwned`); any other canonical requires an explicit `decode`.
    let decode_body = if let Some(decode) = &args.decode {
        quote! { #decode(bytes) }
    } else if is_json {
        quote! { omnifs_sdk::object::decode_json(bytes) }
    } else {
        return Err(syn::Error::new(
            canonical_ident.span(),
            "non-JSON canonical content requires `decode = path::to::function`",
        ));
    };

    // `load` forwards to a provider-written inherent async fn; defaults to
    // `Self::load`.
    let load_path: TokenStream2 = if let Some(path) = &args.load {
        quote! { #path }
    } else {
        quote! { Self::load }
    };

    Ok(quote! {
        #item

        impl #impl_generics omnifs_sdk::object::Object for #struct_ident #ty_generics #where_clause {
            type Key = #object_key;
            type State = crate::__OmnifsProviderState;
            type Canonical = omnifs_sdk::repr::#canonical_ident;

            fn load(
                cx: &omnifs_sdk::Cx<Self::State>,
                key: &Self::Key,
                since: ::core::option::Option<omnifs_sdk::object::Validator>,
            ) -> impl ::core::future::Future<
                Output = omnifs_sdk::error::Result<omnifs_sdk::object::Load<Self>>,
            > {
                #load_path(cx, key, since)
            }

            fn decode(bytes: &[u8]) -> omnifs_sdk::error::Result<Self> {
                #decode_body
            }

            fn kind() -> omnifs_sdk::object::ObjectKind {
                omnifs_sdk::object::ObjectKind(#kind_val)
            }
        }
    })
}
