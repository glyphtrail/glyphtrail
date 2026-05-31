; Definitions
(function_item name: (identifier) @name) @def.function
(struct_item name: (type_identifier) @name) @def.struct
(enum_item name: (type_identifier) @name) @def.enum
(trait_item name: (type_identifier) @name) @def.trait
(mod_item name: (identifier) @name) @def.module

; Calls
(call_expression function: (identifier) @call)
(call_expression function: (scoped_identifier name: (identifier) @call))
(call_expression function: (field_expression field: (field_identifier) @call))
(macro_invocation macro: (identifier) @call)
; Calls nested inside a macro's token tree (e.g. `helper(x)` in
; `format!("{}", helper(x))`): the macro body is raw tokens, so a callee is an
; identifier immediately followed by a parenthesized token tree (#5/#131).
(token_tree (identifier) @call . (token_tree))

; Imports
(use_declaration argument: (_) @import)

; Comments
[(line_comment) (block_comment)] @comment
