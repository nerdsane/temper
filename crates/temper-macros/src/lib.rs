use proc_macro::TokenStream;

/// Derive macro for the Message trait.
/// Automatically implements Message for types that are Send + 'static.
#[proc_macro_derive(Message)]
pub fn derive_message(input: TokenStream) -> TokenStream {
    let ast: syn::DeriveInput = syn::parse(input).unwrap();
    let name = &ast.ident;
    let (impl_generics, ty_generics, where_clause) = ast.generics.split_for_impl();

    let expanded = quote::quote! {
        impl #impl_generics temper_runtime::actor::Message for #name #ty_generics #where_clause {}
    };

    TokenStream::from(expanded)
}

/// Derive macro for DomainEvent trait.
#[proc_macro_derive(DomainEvent)]
pub fn derive_domain_event(input: TokenStream) -> TokenStream {
    let ast: syn::DeriveInput = syn::parse(input).unwrap();
    let name = &ast.ident;
    let (impl_generics, ty_generics, where_clause) = ast.generics.split_for_impl();

    let expanded = quote::quote! {
        impl #impl_generics temper_runtime::persistence::DomainEvent for #name #ty_generics #where_clause {}
    };

    TokenStream::from(expanded)
}
