; Docs: comments directly above a definition, and the `export` keyword in front of it.
((comment)+ @doc . (_) @definition.doc)
((comment)+ @doc . (export_statement "export" @doc . "default"? @doc . declaration: (_) @definition.doc))
(export_statement "export" @doc . "default"? @doc . declaration: (_) @definition.doc)

(function_declaration name: (identifier) @name body: (statement_block) @body) @definition.function
(generator_function_declaration name: (identifier) @name body: (statement_block) @body) @definition.function
(class_declaration name: (identifier) @name) @definition.class
(method_definition name: (_) @name body: (statement_block) @body) @definition.method
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function body: (_) @body) (function_expression body: (_) @body)])) @definition.function

; Matching rules.
";" @match.optional
"," @match.trailing
(string ["\"" "'"] @match.quote (#set! @match.quote alt "\" '"))
(comment) @match.collapse
