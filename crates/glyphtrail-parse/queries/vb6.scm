; Classic Visual Basic (VB6 / VBA). A `.cls`/`.frm` file is itself the class, so
; its procedures are top-level defs.

; Procedures. `Property Get`/`Let`/`Set` share a name and merge into one def.
(sub_declaration name: (identifier) @name) @def.function
(function_declaration name: (identifier) @name) @def.function
(property_declaration name: (identifier) @name) @def.method

; User-defined types, enums, constants.
(type_declaration name: (identifier) @name) @def.struct
(enum_declaration name: (identifier) @name) @def.enum
(const_declaration declarator: (var_declarator name: (identifier) @name)) @def.constant

; `Implements IFoo` in a class module.
(implements_declaration (identifier) @implements)

; Calls: `Foo x`, `Call Foo(x)`, `obj.Foo`, `y = Foo(x)`, `New Foo`. VB uses
; the same `x(i)` syntax for calls and array indexing, so an array read can show
; up as an unresolved call.
(method_invocation target: (name_access) @call)
(method_invocation target: (member_access last: (identifier) @call))
(function_invocation target: (name_access) @call)
(function_invocation target: (member_access last: (identifier) @call))
(instantiation class: (nonprimitive_type (identifier) @call))

; Comments (`'` and `Rem`).
(comment) @comment
