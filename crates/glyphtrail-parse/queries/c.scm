; Definitions
(function_definition
  declarator: (function_declarator declarator: (identifier) @name)) @def.function
(struct_specifier name: (type_identifier) @name) @def.struct
(enum_specifier name: (type_identifier) @name) @def.enum

; Calls
(call_expression function: (identifier) @call)

; Imports
(preproc_include path: (_) @import)

; Comments
(comment) @comment
