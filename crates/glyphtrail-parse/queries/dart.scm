; Definitions
(class_declaration name: (identifier) @name) @def.class
; A named function or method signature (top-level functions and class methods).
(function_signature name: (identifier) @name) @def.function

; Calls
(call_expression function: (identifier) @call)

; Comments
(comment) @comment
