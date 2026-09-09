# Decisions & Tradeoffs

## Decision

Narrow the parser fix to entity decoding failures.

Came up because PR #409 changes raw values to unescape_value().ok(), which silently defaults a present malformed optional attribute.

Options: defer that new defaulting behavior; rewrite all strict parsing; propagate the decoding error alone.

Chose targeted error propagation over broad strict parsing because it resolves the new behavior while preserving the separately planned truncated-schema/startup recovery work.

Where: PR #409, crates/temper-spec/src/csdl/parser/xml.rs.
