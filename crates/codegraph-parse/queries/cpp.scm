; Definitions
(function_definition
  declarator: (function_declarator declarator: (identifier) @name)) @def.function
(function_definition
  declarator: (function_declarator declarator: (qualified_identifier name: (identifier) @name))) @def.method
(class_specifier name: (type_identifier) @name) @def.class
(struct_specifier name: (type_identifier) @name) @def.struct
(enum_specifier name: (type_identifier) @name) @def.enum

; Inheritance
(base_class_clause (type_identifier) @extends)

; Calls
(call_expression function: (identifier) @call)
(call_expression function: (field_expression field: (field_identifier) @call))

; Imports
(preproc_include path: (_) @import)

; Comments
(comment) @comment
