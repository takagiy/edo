; Docs: comments directly above a definition, and the `export` keyword in front of it.
((comment)+ @doc . (_) @definition.doc)
((comment)+ @doc . (export_statement "export" @doc . "default"? @doc . declaration: (_) @definition.doc))
(export_statement "export" @doc . "default"? @doc . declaration: (_) @definition.doc)

(function_declaration name: (identifier) @name body: (statement_block) @body) @definition.function
(generator_function_declaration name: (identifier) @name body: (statement_block) @body) @definition.function
(function_signature name: (identifier) @name) @definition.function
(class_declaration name: (type_identifier) @name) @definition.class
(abstract_class_declaration name: (type_identifier) @name) @definition.class
(method_definition name: (_) @name body: (statement_block) @body) @definition.method
(method_signature name: (_) @name) @definition.method
(abstract_method_signature name: (_) @name) @definition.method
(interface_declaration name: (type_identifier) @name) @definition.interface
(type_alias_declaration name: (type_identifier) @name) @definition.type
(enum_declaration name: (identifier) @name) @definition.enum
(internal_module name: (_) @name) @definition.module
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function body: (_) @body) (function_expression body: (_) @body)])) @definition.function

; Matching rules.
";" @match.optional
"," @match.trailing
(string ["\"" "'"] @match.quote (#set! @match.quote alt "\" '"))
(comment) @match.collapse
