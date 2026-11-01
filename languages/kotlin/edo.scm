; Docs: comments directly above a definition. Annotations are part of the declaration's modifiers.
([(line_comment) (block_comment)]+ @doc . (_) @definition.doc)

(class_declaration name: (identifier) @name) @definition.class
(object_declaration name: (identifier) @name) @definition.object
(companion_object name: (identifier) @name) @definition.object
((companion_object !name) @definition.object (#set! name "Companion"))
(function_declaration name: (identifier) @name (function_body (_) @body)) @definition.function
(function_declaration name: (identifier) @name) @definition.function
(secondary_constructor (block) @body) @definition.constructor
(type_alias type: (identifier) @name) @definition.type

; Matching rules. 'a' is a char, so quotes are not relaxed.
";" @match.optional
"," @match.trailing
[(line_comment) (block_comment)] @match.collapse
