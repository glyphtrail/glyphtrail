; Definitions: a function bound to a name, `f <- function(...) { ... }`. The
; definition is the whole assignment; its name is the left-hand identifier.
(binary_operator lhs: (identifier) @name rhs: (function_definition)) @def.function

; Calls
(call function: (identifier) @call)

; Comments
(comment) @comment
