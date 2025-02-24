#![recursion_limit = "128"]
use if_chain::if_chain;
use proc_macro2::Span;
use proc_macro_error::{abort, proc_macro_error};
use quote::quote;
use quote::ToTokens;
use std::collections::HashMap;
use syn::{parse_quote, spanned::Spanned};
use validator_types::Validator;

mod asserts;
mod lit;
mod quoting;
mod validation;

use asserts::{assert_has_len, assert_has_range, assert_string_type, assert_type_matches};
use lit::*;
use quoting::{quote_field_validation, quote_schema_validations, FieldQuoter};
use validation::*;

#[proc_macro_derive(Validate, attributes(validate))]
#[proc_macro_error]
pub fn derive_validation(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let ast = syn::parse(input).unwrap();
    impl_validate(&ast).into()
}

fn impl_validate(ast: &syn::DeriveInput) -> proc_macro2::TokenStream {
    // Ensure the macro is on a struct with named fields
    let fields = match ast.data {
        syn::Data::Struct(syn::DataStruct { ref fields, .. }) => {
            if fields.iter().any(|field| field.ident.is_none()) {
                abort!(
                    fields.span(),
                    "struct has unnamed fields";
                    help = "#[derive(Validate)] can only be used on structs with named fields";
                );
            }
            fields.iter().cloned().collect::<Vec<_>>()
        }
        _ => abort!(ast.span(), "#[derive(Validate)] can only be used with structs"),
    };

    let mut validations = vec![];
    let mut nested_validations = vec![];

    let field_types = find_fields_type(&fields);

    for field in &fields {
        let field_ident = field.ident.clone().unwrap();
        let (name, field_validations) = find_validators_for_field(field, &field_types);
        let field_type = field_types.get(&field_ident.to_string()).cloned().unwrap();
        let field_quoter = FieldQuoter::new(field_ident, name, field_type);

        for validation in &field_validations {
            quote_field_validation(
                &field_quoter,
                validation,
                &mut validations,
                &mut nested_validations,
            );
        }
    }

    let schema_validations = quote_schema_validations(&find_struct_validations(&ast.attrs));

    let ident = &ast.ident;

    // Helper is provided for handling complex generic types correctly and effortlessly
    let (impl_generics, ty_generics, where_clause) = ast.generics.split_for_impl();
    let impl_ast = quote!(
        impl #impl_generics ::validator::Validate for #ident #ty_generics #where_clause {
            #[allow(unused_mut)]
            fn validate(&self) -> ::std::result::Result<(), ::validator::ValidationErrors> {
                let mut errors = ::validator::ValidationErrors::new();

                #(#validations)*

                #(#schema_validations)*

                let mut result = if errors.is_empty() {
                    ::std::result::Result::Ok(())
                } else {
                    ::std::result::Result::Err(errors)
                };

                #(#nested_validations)*
                result
            }
        }
    );
    // println!("{}", impl_ast.to_string());
    impl_ast
}

/// Find if a struct has some schema validation and returns the info if so
fn find_struct_validation(attr: &syn::Attribute) -> SchemaValidation {
    let error = |span: Span, msg: &str| -> ! {
        abort!(span, "Invalid schema level validation: {}", msg);
    };

    if_chain! {
        if let Ok(syn::Meta::List(syn::MetaList { ref nested, .. })) = attr.parse_meta();
        if let syn::NestedMeta::Meta(ref item) = nested[0];
        if let syn::Meta::List(syn::MetaList { ref path, ref nested, .. }) = *item;

        then {
            let ident = path.get_ident().unwrap();
            if ident != "schema" {
                error(attr.span(), "Only `schema` is allowed as validator on a struct")
            }

            let mut function = String::new();
            let mut skip_on_field_errors = true;
            let mut code = None;
            let mut message = None;

            for arg in nested {
                if_chain! {
                    if let syn::NestedMeta::Meta(ref item) = *arg;
                    if let syn::Meta::NameValue(syn::MetaNameValue { ref path, ref lit, .. }) = *item;

                    then {
                        let ident = path.get_ident().unwrap();
                        match ident.to_string().as_ref() {
                            "function" => {
                                function = match lit_to_string(lit) {
                                    Some(s) => s,
                                    None => error(lit.span(), "invalid argument type for `function` \
                                    : only a string is allowed"),
                                };
                            },
                            "skip_on_field_errors" => {
                                skip_on_field_errors = match lit_to_bool(lit) {
                                    Some(s) => s,
                                    None => error(lit.span(), "invalid argument type for `skip_on_field_errors` \
                                    : only a bool is allowed"),
                                };
                            },
                            "code" => {
                                code = match lit_to_string(lit) {
                                    Some(s) => Some(s),
                                    None => error(lit.span(), "invalid argument type for `code` \
                                    : only a string is allowed"),
                                };
                            },
                            "message" => {
                                message = match lit_to_string(lit) {
                                    Some(s) => Some(s),
                                    None => error(lit.span(), "invalid argument type for `message` \
                                    : only a string is allowed"),
                                };
                            },
                            _ => error(lit.span(), "Unknown argument")
                        }
                    } else {
                        error(arg.span(), "Unexpected args")
                    }
                }
            }

            if function.is_empty() {
                error(path.span(), "`function` is required");
            }

            SchemaValidation {
                function,
                skip_on_field_errors,
                code,
                message,
            }
        } else {
            error(attr.span(), "Unexpected struct validator")
        }
    }
}

/// Finds all struct schema validations
fn find_struct_validations(struct_attrs: &[syn::Attribute]) -> Vec<SchemaValidation> {
    struct_attrs
        .iter()
        .filter(|attribute| attribute.path == parse_quote!(validate))
        .map(|attribute| find_struct_validation(attribute))
        .collect()
}

/// Find the types (as string) for each field of the struct
/// Needed for the `must_match` filter
fn find_fields_type(fields: &[syn::Field]) -> HashMap<String, String> {
    let mut types = HashMap::new();

    for field in fields {
        let field_ident = field.ident.clone().unwrap().to_string();
        let field_type = match field.ty {
            syn::Type::Path(syn::TypePath { ref path, .. }) => {
                let mut tokens = proc_macro2::TokenStream::new();
                path.to_tokens(&mut tokens);
                tokens.to_string().replace(' ', "")
            }
            syn::Type::Reference(syn::TypeReference { ref lifetime, ref elem, .. }) => {
                let mut tokens = proc_macro2::TokenStream::new();
                elem.to_tokens(&mut tokens);
                let mut name = tokens.to_string().replace(' ', "");
                if lifetime.is_some() {
                    name.insert(0, '&')
                }
                name
            }
            syn::Type::Group(syn::TypeGroup { ref elem, .. }) => {
                let mut tokens = proc_macro2::TokenStream::new();
                elem.to_tokens(&mut tokens);
                tokens.to_string().replace(' ', "")
            }
            _ => {
                let mut field_type = proc_macro2::TokenStream::new();
                field.ty.to_tokens(&mut field_type);
                abort!(
                    field.ty.span(),
                    "Type `{}` of field `{}` not supported",
                    field_type,
                    field_ident
                )
            }
        };

        //println!("{:?}", field_type);
        types.insert(field_ident, field_type);
    }

    types
}

/// Find everything we need to know about a field: its real name if it's changed from the serialization
/// and the list of validators to run on it
fn find_validators_for_field(
    field: &syn::Field,
    field_types: &HashMap<String, String>,
) -> (String, Vec<FieldValidation>) {
    let rust_ident = field.ident.clone().unwrap().to_string();
    let mut field_ident = field.ident.clone().unwrap().to_string();

    let error = |span: Span, msg: &str| -> ! {
        abort!(
            span,
            "Invalid attribute #[validate] on field `{}`: {}",
            field.ident.clone().unwrap().to_string(),
            msg
        );
    };

    let field_type = field_types.get(&field_ident).unwrap();

    let mut validators = vec![];
    let mut has_validate = false;

    for attr in &field.attrs {
        if attr.path != parse_quote!(validate) && attr.path != parse_quote!(serde) {
            continue;
        }

        if attr.path == parse_quote!(validate) {
            has_validate = true;
        }

        match attr.parse_meta() {
            Ok(syn::Meta::List(syn::MetaList { ref nested, .. })) => {
                let meta_items = nested.iter().collect::<Vec<_>>();
                // original name before serde rename
                if attr.path == parse_quote!(serde) {
                    if let Some(s) = find_original_field_name(&meta_items) {
                        field_ident = s;
                    }
                    continue;
                }

                // only validation from there on
                for meta_item in meta_items {
                    match *meta_item {
                        syn::NestedMeta::Meta(ref item) => match *item {
                            // email, url, phone, credit_card, non_control_character
                            syn::Meta::Path(ref name) => {
                                match name.get_ident().unwrap().to_string().as_ref() {
                                    "email" => {
                                        assert_string_type("email", field_type, &field.ty);
                                        validators.push(FieldValidation::new(Validator::Email));
                                    }
                                    "url" => {
                                        assert_string_type("url", field_type, &field.ty);
                                        validators.push(FieldValidation::new(Validator::Url));
                                    }
                                    #[cfg(feature = "phone")]
                                    "phone" => {
                                        assert_string_type("phone", field_type, &field.ty);
                                        validators.push(FieldValidation::new(Validator::Phone));
                                    }
                                    #[cfg(feature = "card")]
                                    "credit_card" => {
                                        assert_string_type("credit_card", field_type, &field.ty);
                                        validators
                                            .push(FieldValidation::new(Validator::CreditCard));
                                    }
                                    #[cfg(feature = "unic")]
                                    "non_control_character" => {
                                        assert_string_type(
                                            "non_control_character",
                                            field_type,
                                            &field.ty,
                                        );
                                        validators.push(FieldValidation::new(
                                            Validator::NonControlCharacter,
                                        ));
                                    }
                                    "required" => {
                                        validators.push(FieldValidation::new(Validator::Required));
                                    }
                                    "required_nested" => {
                                        validators.push(FieldValidation::new(Validator::Required));
                                        validators.push(FieldValidation::new(Validator::Nested));
                                    }
                                    _ => {
                                        let mut ident = proc_macro2::TokenStream::new();
                                        name.to_tokens(&mut ident);
                                        abort!(name.span(), "Unexpected validator: {}", ident)
                                    }
                                }
                            }
                            // custom, contains, must_match, regex
                            syn::Meta::NameValue(syn::MetaNameValue {
                                ref path, ref lit, ..
                            }) => {
                                let ident = path.get_ident().unwrap();
                                match ident.to_string().as_ref() {
                                    "custom" => {
                                        match lit_to_string(lit) {
                                            Some(s) => validators.push(FieldValidation::new(Validator::Custom(s))),
                                            None => error(lit.span(), "invalid argument for `custom` validator: only strings are allowed"),
                                        };
                                    }
                                    "contains" => {
                                        match lit_to_string(lit) {
                                            Some(s) => validators.push(FieldValidation::new(Validator::Contains(s))),
                                            None => error(lit.span(), "invalid argument for `contains` validator: only strings are allowed"),
                                        };
                                    }
                                    "regex" => {
                                        match lit_to_string(lit) {
                                            Some(s) => validators.push(FieldValidation::new(Validator::Regex(s))),
                                            None => error(lit.span(), "invalid argument for `regex` validator: only strings are allowed"),
                                        };
                                    }
                                    "must_match" => {
                                        match lit_to_string(lit) {
                                            Some(s) => {
                                                assert_type_matches(rust_ident.clone(), field_type, field_types.get(&s), &attr);
                                                validators.push(FieldValidation::new(Validator::MustMatch(s)));
                                            },
                                            None => error(lit.span(), "invalid argument for `must_match` validator: only strings are allowed"),
                                        };
                                    }
                                    v => abort!(
                                        path.span(),
                                        "unexpected name value validator: {:?}",
                                        v
                                    ),
                                };
                            }
                            // Validators with several args
                            syn::Meta::List(syn::MetaList { ref path, ref nested, .. }) => {
                                let meta_items = nested.iter().cloned().collect::<Vec<_>>();
                                let ident = path.get_ident().unwrap();
                                match ident.to_string().as_ref() {
                                    "length" => {
                                        assert_has_len(rust_ident.clone(), field_type, &field.ty);
                                        validators.push(extract_length_validation(
                                            rust_ident.clone(),
                                            attr,
                                            &meta_items,
                                        ));
                                    }
                                    "range" => {
                                        assert_has_range(rust_ident.clone(), field_type, &field.ty);
                                        validators.push(extract_range_validation(
                                            rust_ident.clone(),
                                            attr,
                                            &meta_items,
                                        ));
                                    }
                                    "email"
                                    | "url"
                                    | "phone"
                                    | "credit_card"
                                    | "non_control_character" => {
                                        validators.push(extract_argless_validation(
                                            ident.to_string(),
                                            rust_ident.clone(),
                                            &meta_items,
                                        ));
                                    }
                                    "custom" => {
                                        validators.push(extract_one_arg_validation(
                                            "function",
                                            ident.to_string(),
                                            rust_ident.clone(),
                                            &meta_items,
                                        ));
                                    }
                                    "contains" => {
                                        validators.push(extract_one_arg_validation(
                                            "pattern",
                                            ident.to_string(),
                                            rust_ident.clone(),
                                            &meta_items,
                                        ));
                                    }
                                    "regex" => {
                                        validators.push(extract_one_arg_validation(
                                            "path",
                                            ident.to_string(),
                                            rust_ident.clone(),
                                            &meta_items,
                                        ));
                                    }
                                    "must_match" => {
                                        let validation = extract_one_arg_validation(
                                            "other",
                                            ident.to_string(),
                                            rust_ident.clone(),
                                            &meta_items,
                                        );
                                        if let Validator::MustMatch(ref t2) = validation.validator {
                                            assert_type_matches(
                                                rust_ident.clone(),
                                                field_type,
                                                field_types.get(t2),
                                                &attr,
                                            );
                                        }
                                        validators.push(validation);
                                    }
                                    v => abort!(path.span(), "unexpected list validator: {:?}", v),
                                }
                            }
                        },
                        _ => unreachable!("Found a non Meta while looking for validators"),
                    };
                }
            }
            Ok(syn::Meta::Path(_)) => validators.push(FieldValidation::new(Validator::Nested)),
            Ok(syn::Meta::NameValue(_)) => abort!(attr.span(), "Unexpected name=value argument"),
            Err(e) => unreachable!(
                "Got something other than a list of attributes while checking field `{}`: {:?}",
                field_ident, e
            ),
        }

        if has_validate && validators.is_empty() {
            error(attr.span(), "it needs at least one validator");
        }
    }

    (field_ident, validators)
}

/// Serde can be used to rename fields on deserialization but most of the times
/// we want the error on the original field.
///
/// For example a JS frontend might send camelCase fields and Rust converts them to snake_case
/// but we want to send the errors back with the original name
fn find_original_field_name(meta_items: &[&syn::NestedMeta]) -> Option<String> {
    let mut original_name = None;

    for meta_item in meta_items {
        match **meta_item {
            syn::NestedMeta::Meta(ref item) => match *item {
                syn::Meta::Path(_) => continue,
                syn::Meta::NameValue(syn::MetaNameValue { ref path, ref lit, .. }) => {
                    let ident = path.get_ident().unwrap();
                    if ident == "rename" {
                        original_name = Some(lit_to_string(lit).unwrap());
                    }
                }
                syn::Meta::List(syn::MetaList { ref nested, .. }) => {
                    return find_original_field_name(&nested.iter().collect::<Vec<_>>());
                }
            },
            _ => unreachable!(),
        };

        if original_name.is_some() {
            return original_name;
        }
    }

    original_name
}
