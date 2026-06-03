; Definitions
(function_item name: (identifier) @name) @def.function
(struct_item name: (type_identifier) @name) @def.struct
(enum_item name: (type_identifier) @name) @def.enum
(trait_item name: (type_identifier) @name) @def.trait
(mod_item name: (identifier) @name) @def.module
; `const`/`static` items are value definitions, so `definition`/`search` resolve
; them and `neighbors` reaches them via file containment (#453).
(const_item name: (identifier) @name) @def.constant
(static_item name: (identifier) @name) @def.constant

; Calls
(call_expression function: (identifier) @call)
(call_expression function: (scoped_identifier name: (identifier) @call))
(call_expression function: (field_expression field: (field_identifier) @call))
(macro_invocation macro: (identifier) @call)
; Calls nested inside a macro's token tree (e.g. `helper(x)` in
; `format!("{}", helper(x))`): the macro body is raw tokens, so a callee is an
; identifier immediately followed by a parenthesized token tree (#5/#131).
(token_tree (identifier) @call . (token_tree))

; References (type usages, not calls): a type named in a signature, field,
; generic, or impl, and the path prefix of a scoped value like `Protocol::Rest`
; or `Protocol::from_str`. These become References edges so `impact` and
; neighbour queries reach a type's users, not only its callers (#310). A
; type_identifier that is a definition's own name is filtered out downstream.
(type_identifier) @ref
(scoped_identifier path: (identifier) @ref)

; Imports
(use_declaration argument: (_) @import)

; Comments
[(line_comment) (block_comment)] @comment
