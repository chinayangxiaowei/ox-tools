// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Recognizes generated reference leaks without treating literal text as code.

use proc_macro2::{Delimiter, TokenStream, TokenTree};

/// Finds a generated leak call, including one nested in a macro, without matching source text in
/// comments or string literals.
#[must_use]
pub fn generated_leak_call(replacement: &str) -> bool {
    if !replacement.contains("leak") {
        return false;
    }
    let Ok(tokens) = replacement.parse::<TokenStream>() else {
        return false;
    };

    leak_call_tokens(tokens)
}

fn leak_call_tokens(tokens: TokenStream) -> bool {
    let tokens = tokens.into_iter().collect::<Vec<_>>();
    let call = tokens.windows(5).any(|window| {
        matches!(&window[0], TokenTree::Ident(name) if matches!(name.to_string().as_str(), "Box" | "Vec" | "r#Box" | "r#Vec"))
            && matches!(&window[1], TokenTree::Punct(colon) if colon.as_char() == ':')
            && matches!(&window[2], TokenTree::Punct(colon) if colon.as_char() == ':')
            && matches!(&window[3], TokenTree::Ident(name) if matches!(name.to_string().as_str(), "leak" | "r#leak"))
            && matches!(&window[4], TokenTree::Group(args) if args.delimiter() == Delimiter::Parenthesis)
    });

    call || tokens
        .into_iter()
        .any(|token| matches!(token, TokenTree::Group(group) if leak_call_tokens(group.stream())))
}

#[cfg(test)]
mod tests {
    use super::generated_leak_call;

    #[test]
    fn generated_leak_calls_ignore_strings_and_comments() {
        assert!(generated_leak_call("Some(&*Box::leak(Box::new(0)))"));
        assert!(generated_leak_call("wrapper!(Vec::leak(vec![0]))"));
        assert!(!generated_leak_call("&mut []"));
        assert!(!generated_leak_call("\"Box::leak(\" /* Vec::leak( */"));
    }
}
