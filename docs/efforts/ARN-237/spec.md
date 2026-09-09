# ARN-237: spec

CSDL emission escapes every attribute and preserves values across a parse round trip. Valid entities decode. Invalid entity references must not turn a present optional attribute into an absent/defaulted value. This correction does not add mandatory-attribute, truncated-document, numeric-validation, or startup-recovery features.
