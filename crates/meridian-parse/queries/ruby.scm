; Definitions
(class name: (constant) @name) @def.class
(module name: (constant) @name) @def.module
(method name: (identifier) @name) @def.method
(singleton_method name: (identifier) @name) @def.method

; Calls (explicit receiver/parenthesized calls; bare `foo` is an ambiguous
; identifier in Ruby and is not captured).
(call method: (identifier) @call)

; Comments
(comment) @comment
