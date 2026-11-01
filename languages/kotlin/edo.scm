; Docs: comments directly above a definition. Annotations are part of the declaration's modifiers.
([(line_comment) (multiline_comment)]+ @doc . (_) @definition.doc)

(class_declaration (type_identifier) @name) @definition.class
(object_declaration (type_identifier) @name) @definition.object
(companion_object (type_identifier) @name) @definition.object
((companion_object) @definition.object (#set! name "Companion"))
; The body is what follows `=` or sits between the braces.
(function_declaration (simple_identifier) @name (function_body (_) @body)) @definition.function
(function_declaration (simple_identifier) @name) @definition.function
(type_alias (type_identifier) @name) @definition.type

; Matching rules. 'a' is a char, so quotes are not relaxed.
";" @match.optional
"," @match.trailing
[(line_comment) (multiline_comment)] @match.collapse
