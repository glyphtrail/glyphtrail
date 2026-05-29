; Definitions — a let binding's bound name (the first value_name child).
(let_binding (value_name) @name) @def.function

; Calls — the function position (first child) of an application.
(application_expression . (value_path (value_name) @call))

; Comments
(comment) @comment
