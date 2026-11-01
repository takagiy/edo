; Docs: comments and attributes directly above a definition.
([(line_comment) (block_comment) (attribute_item)]+ @doc . (_) @definition.doc)

(function_item name: (identifier) @name body: (block) @body) @definition.function
(function_signature_item name: (identifier) @name) @definition.function
(struct_item name: (type_identifier) @name) @definition.struct
(enum_item name: (type_identifier) @name) @definition.enum
(union_item name: (type_identifier) @name) @definition.union
(trait_item name: (type_identifier) @name) @definition.trait
(impl_item type: (_) @name) @definition.impl
(mod_item name: (identifier) @name) @definition.module
(type_item name: (type_identifier) @name) @definition.type
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.constant
(macro_definition name: (identifier) @name) @definition.macro

; Matching rules. `;` changes meaning and 'a' is a char, so neither is relaxed.
"," @match.trailing
[(line_comment) (block_comment)] @match.collapse
