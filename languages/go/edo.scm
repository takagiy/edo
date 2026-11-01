; Docs: comments directly above a definition.
((comment)+ @doc . (_) @definition.doc)

(function_declaration name: (identifier) @name body: (block) @body) @definition.function
(method_declaration name: (field_identifier) @name body: (block) @body) @definition.method
(type_declaration (type_spec name: (type_identifier) @name)) @definition.type
(method_elem name: (field_identifier) @name) @definition.method

; Matching rules. "..." and `...` are different literals, so quotes are not relaxed.
"," @match.trailing
(comment) @match.collapse
