; Functions: `fn name(...) ReturnType { ... }` (the `name` identifier).
(function_declaration . (identifier) @name) @def.function

; Calls — the callee identifier of a `name(...)` or `recv.method(...)` call.
(call_expression . (identifier) @call)
(call_expression . (field_expression (identifier) @call .))

; Comments (`//`, `///` doc comments).
(comment) @comment
