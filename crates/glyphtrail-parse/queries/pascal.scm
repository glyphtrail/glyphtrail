; Pascal (Free Pascal / Delphi). A routine is declared up to twice (a forward or
; interface-section header, then `defProc` with the body), so functions and
; methods are taken from the body only — one def per routine keeps local call
; resolution unambiguous.

; Types: `TFoo = class(...)`, `record`, `interface`, helper, enum `(A, B)`.
; Generic names (`TList<T>`) wrap the identifier in `genericTpl`.
(declType name: [(identifier) @name (genericTpl entity: (identifier) @name)] type: (declClass (kClass))) @def.class
(declType name: [(identifier) @name (genericTpl entity: (identifier) @name)] type: (declClass (kObject))) @def.class
(declType name: [(identifier) @name (genericTpl entity: (identifier) @name)] type: (declClass (kRecord))) @def.struct
(declType name: [(identifier) @name (genericTpl entity: (identifier) @name)] type: (declIntf)) @def.interface
; Delphi `class helper for X` / `record helper for X` adds methods to X.
(declType name: (identifier) @name type: (declHelper)) @def.class
(declType name: (identifier) @name type: (type (declEnum))) @def.enum

; Interface methods have no body anywhere, so their declaration is the def.
(declIntf (declProc name: (identifier) @name) @def.method)

; Routines: `procedure Foo; begin ... end;` and the method body
; `procedure TFoo.Bar; begin ... end;` (named by its last segment).
(defProc header: (declProc name: [(identifier) @name (genericTpl entity: (identifier) @name)])) @def.function
(defProc header: (declProc name: (genericDot rhs: [(identifier) @name (genericTpl entity: (identifier) @name)]))) @def.method

; Inheritance: `class(TBase, IFoo)` lists the base class and interfaces together.
(declClass parent: (typeref [(identifier) @extends (typerefTpl entity: (identifier) @extends)]))
(declIntf parent: (typeref [(identifier) @extends (typerefTpl entity: (identifier) @extends)]))

; Calls: `Foo(...)`, `obj.Foo(...)`, `Foo<T>(...)`, and the parameterless
; statement forms `Foo;` / `obj.Foo;`.
(exprCall entity: (identifier) @call)
(exprCall entity: (exprDot rhs: (identifier) @call))
(exprCall entity: (exprTpl entity: (identifier) @call))
(statement (identifier) @call)
(statement (exprDot rhs: (identifier) @call))

; Imports: `uses SysUtils, Geometry.Vectors;`
(declUses (moduleName) @import)

; Comments: `//`, `{ }`, `(* *)`.
(comment) @comment
