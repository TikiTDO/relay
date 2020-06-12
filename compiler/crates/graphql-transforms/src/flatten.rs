/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::util::{
    is_relay_custom_inline_fragment_directive, PointerAddress, CUSTOM_METADATA_DIRECTIVES,
};
use graphql_ir::{
    Condition, Directive, FragmentDefinition, InlineFragment, LinkedField, OperationDefinition,
    Program, ScalarField, Selection,
};
use schema::TypeReference;

use crate::node_identifier::{LocationAgnosticPartialEq, NodeIdentifier};
use common::NamedItem;
use fnv::FnvHashMap;
use parking_lot::RwLock;
use rayon::prelude::*;
use schema::Schema;
use std::sync::Arc;

type SeenLinkedFields = Arc<RwLock<FnvHashMap<PointerAddress, Arc<LinkedField>>>>;

///
/// Transform that flattens inline fragments, fragment spreads, merges linked fields selections.
///
/// Inline fragments are inlined (replaced with their selections) when:
/// - The fragment type matches the type of its parent, and it `is_for_codegen`,
///   or the inline fragment doesn't have directives .
///
/// with the exception that it never flattens the inline fragment with relay
/// directives (@defer, @__clientExtensions).
///
pub fn flatten(program: &Program, is_for_codegen: bool) -> Program {
    let next_program = Arc::new(RwLock::new(Program::new(Arc::clone(&program.schema))));
    let transform = FlattenTransform::new(program, is_for_codegen);

    program.par_operations().for_each(|operation| {
        let operation = Arc::new(transform.transform_operation(operation));
        {
            let mut next_program = next_program.write();
            next_program.insert_operation(operation);
        }
    });
    program.par_fragments().for_each(|fragment| {
        let fragment = Arc::new(transform.transform_fragment(fragment));
        {
            let mut next_program = next_program.write();
            next_program.insert_fragment(fragment);
        }
    });
    Arc::try_unwrap(next_program).unwrap().into_inner()
}

struct FlattenTransform {
    schema: Arc<Schema>,
    is_for_codegen: bool,
    seen_linked_fields: SeenLinkedFields,
}

impl FlattenTransform {
    fn new(program: &'_ Program, is_for_codegen: bool) -> Self {
        Self {
            schema: Arc::clone(&program.schema),
            is_for_codegen,
            seen_linked_fields: Default::default(),
        }
    }

    fn transform_operation(&self, operation: &OperationDefinition) -> OperationDefinition {
        OperationDefinition {
            kind: operation.kind,
            name: operation.name,
            type_: operation.type_,
            directives: operation.directives.clone(),
            variable_definitions: operation.variable_definitions.clone(),
            selections: self.tranform_selections(
                &operation.selections,
                &TypeReference::Named(operation.type_),
            ),
        }
    }

    fn transform_fragment(&self, fragment: &FragmentDefinition) -> FragmentDefinition {
        FragmentDefinition {
            name: fragment.name,
            type_condition: fragment.type_condition,
            directives: fragment.directives.clone(),
            variable_definitions: fragment.variable_definitions.clone(),
            used_global_variables: fragment.used_global_variables.clone(),
            selections: self.tranform_selections(
                &fragment.selections,
                &TypeReference::Named(fragment.type_condition),
            ),
        }
    }

    fn tranform_selections(
        &self,
        selections: &[Selection],
        parent_type: &TypeReference,
    ) -> Vec<Selection> {
        let next_selections = selections
            .iter()
            .map(|s| self.transform_selection(s, parent_type))
            .collect::<Vec<_>>();
        let mut flattened_selections = Vec::with_capacity(next_selections.len());
        self.flatten_selections(&mut flattened_selections, &next_selections, parent_type);

        flattened_selections
    }

    fn transform_linked_field(&self, linked_field: &Arc<LinkedField>) -> Arc<LinkedField> {
        let key = PointerAddress::new(Arc::as_ref(linked_field));
        {
            let seen_linked_fields = self.seen_linked_fields.read();
            if let Some(prev) = seen_linked_fields.get(&key) {
                return Arc::clone(prev);
            }
        }
        let type_ = &self
            .schema
            .field(linked_field.definition.item)
            .type_
            .clone();
        let result = Arc::new(LinkedField {
            alias: linked_field.alias,
            definition: linked_field.definition,
            arguments: linked_field.arguments.clone(),
            directives: linked_field.directives.clone(),
            selections: self.tranform_selections(&linked_field.selections, &type_),
        });
        let mut seen_linked_fields = self.seen_linked_fields.write();
        // If another thread computed this in the meantime, use that result
        if let Some(prev) = seen_linked_fields.get(&key) {
            return Arc::clone(prev);
        }
        seen_linked_fields.insert(key, Arc::clone(&result));
        result
    }

    fn transform_selection(&self, selection: &Selection, parent_type: &TypeReference) -> Selection {
        match selection {
            Selection::InlineFragment(node) => {
                let next_parent_type: TypeReference = match node.type_condition {
                    Some(type_condition) => TypeReference::Named(type_condition),
                    None => parent_type.clone(),
                };
                Selection::InlineFragment(Arc::new(InlineFragment {
                    type_condition: node.type_condition,
                    directives: node.directives.clone(),
                    selections: self.tranform_selections(&node.selections, &next_parent_type),
                }))
            }
            Selection::LinkedField(node) => {
                Selection::LinkedField(self.transform_linked_field(node))
            }
            Selection::Condition(node) => Selection::Condition(Arc::new(Condition {
                value: node.value.clone(),
                passing_value: node.passing_value,
                selections: self.tranform_selections(&node.selections, parent_type),
            })),
            Selection::FragmentSpread(node) => Selection::FragmentSpread(Arc::clone(node)),
            Selection::ScalarField(node) => Selection::ScalarField(Arc::clone(node)),
        }
    }

    fn flatten_selections(
        &self,
        flattened_selections: &mut Vec<Selection>,
        selections: &[Selection],
        parent_type: &TypeReference,
    ) {
        for selection in selections {
            if let Selection::InlineFragment(inline_fragment) = selection {
                if should_flatten_inline_fragment(inline_fragment, parent_type, self.is_for_codegen)
                {
                    self.flatten_selections(
                        flattened_selections,
                        &inline_fragment.selections,
                        parent_type,
                    );
                    continue;
                }
            }

            let flattened_selection = flattened_selections
                .iter_mut()
                .find(|sel| NodeIdentifier::are_equal(&self.schema, sel, selection));

            match flattened_selection {
                None => {
                    flattened_selections.push(selection.clone());
                }
                Some(flattened_selection) => {
                    match flattened_selection {
                        Selection::InlineFragment(flattened_node) => {
                            let type_condition: TypeReference = match flattened_node.type_condition
                            {
                                Some(type_condition) => TypeReference::Named(type_condition),
                                None => parent_type.clone(),
                            };

                            let node_selections = match selection {
                                Selection::InlineFragment(node) => &node.selections,
                                _ => unreachable!("FlattenTransform: Expected an InlineFragment."),
                            };

                            *flattened_selection =
                                Selection::InlineFragment(Arc::new(InlineFragment {
                                    type_condition: flattened_node.type_condition,
                                    directives: flattened_node.directives.clone(),
                                    selections: self.merge_selections(
                                        &flattened_node.selections,
                                        &node_selections,
                                        &type_condition,
                                    ),
                                }));
                        }
                        Selection::LinkedField(flattened_node) => {
                            let node_selections = match selection {
                                Selection::LinkedField(node) => &node.selections,
                                _ => unreachable!("FlattenTransform: Expected a LinkedField."),
                            };
                            let type_ = &self
                                .schema
                                .field(flattened_node.definition.item)
                                .type_
                                .clone();
                            let should_merge_handles = selection.directives().iter().any(|d| {
                                CUSTOM_METADATA_DIRECTIVES.is_handle_field_directive(d.name.item)
                            });
                            let next_directives = if should_merge_handles {
                                merge_handle_directives(
                                    &flattened_node.directives,
                                    selection.directives(),
                                )
                            } else {
                                flattened_node.directives.clone()
                            };
                            *flattened_selection = Selection::LinkedField(Arc::new(LinkedField {
                                alias: flattened_node.alias,
                                definition: flattened_node.definition,
                                arguments: flattened_node.arguments.clone(),
                                directives: next_directives,
                                selections: self.merge_selections(
                                    &flattened_node.selections,
                                    &node_selections,
                                    &type_,
                                ),
                            }));
                        }
                        Selection::Condition(flattened_node) => {
                            let node_selections = match selection {
                                Selection::Condition(node) => &node.selections,
                                _ => unreachable!("FlattenTransform: Expected a Condition."),
                            };

                            *flattened_selection = Selection::Condition(Arc::new(Condition {
                                value: flattened_node.value.clone(),
                                passing_value: flattened_node.passing_value,
                                selections: self.merge_selections(
                                    &flattened_node.selections,
                                    &node_selections,
                                    parent_type,
                                ),
                            }));
                        }
                        Selection::ScalarField(flattened_node) => {
                            let should_merge_handles = selection.directives().iter().any(|d| {
                                CUSTOM_METADATA_DIRECTIVES.is_handle_field_directive(d.name.item)
                            });
                            if should_merge_handles {
                                *flattened_selection =
                                    Selection::ScalarField(Arc::new(ScalarField {
                                        alias: flattened_node.alias,
                                        definition: flattened_node.definition,
                                        arguments: flattened_node.arguments.clone(),
                                        directives: merge_handle_directives(
                                            &flattened_node.directives,
                                            selection.directives(),
                                        ),
                                    }))
                            }
                        }
                        Selection::FragmentSpread(_) => {}
                    };
                }
            }
        }
    }

    fn merge_selections(
        &self,
        selections_a: &[Selection],
        selections_b: &[Selection],
        parent_type: &TypeReference,
    ) -> Vec<Selection> {
        let mut flattened_selections = Vec::with_capacity(selections_a.len());
        self.flatten_selections(&mut flattened_selections, selections_a, parent_type);
        self.flatten_selections(&mut flattened_selections, selections_b, parent_type);
        flattened_selections
    }
}

fn should_flatten_inline_with_directives(directives: &[Directive], is_for_codegen: bool) -> bool {
    if is_for_codegen {
        !directives
            .iter()
            .any(is_relay_custom_inline_fragment_directive)
    } else {
        directives.is_empty()
    }
}

fn should_flatten_inline_fragment(
    inline_fragment: &InlineFragment,
    parent_type: &TypeReference,
    is_for_codegen: bool,
) -> bool {
    match inline_fragment.type_condition {
        None => should_flatten_inline_with_directives(&inline_fragment.directives, is_for_codegen),
        Some(type_condition) => {
            type_condition == parent_type.inner()
                && should_flatten_inline_with_directives(
                    &inline_fragment.directives,
                    is_for_codegen,
                )
        }
    }
}

fn merge_handle_directives(
    directives_a: &[Directive],
    directives_b: &[Directive],
) -> Vec<Directive> {
    let (mut handles, mut directives): (Vec<_>, Vec<_>) =
        directives_a.iter().cloned().partition(|directive| {
            CUSTOM_METADATA_DIRECTIVES.is_handle_field_directive(directive.name.item)
        });
    for directive in directives_b {
        if CUSTOM_METADATA_DIRECTIVES.is_handle_field_directive(directive.name.item) {
            if handles.is_empty() {
                handles.push(directive.clone());
            } else {
                let current_handler_arg = directive.arguments.named(
                    CUSTOM_METADATA_DIRECTIVES
                        .handle_field_constants
                        .handler_arg_name,
                );
                let current_name_arg = directive.arguments.named(
                    CUSTOM_METADATA_DIRECTIVES
                        .handle_field_constants
                        .key_arg_name,
                );
                let is_duplicate_handle = handles.iter().any(|handle| {
                    current_handler_arg.location_agnostic_eq(
                        &handle.arguments.named(
                            CUSTOM_METADATA_DIRECTIVES
                                .handle_field_constants
                                .handler_arg_name,
                        ),
                    ) && current_name_arg.location_agnostic_eq(
                        &handle.arguments.named(
                            CUSTOM_METADATA_DIRECTIVES
                                .handle_field_constants
                                .key_arg_name,
                        ),
                    )
                });
                if !is_duplicate_handle {
                    handles.push(directive.clone());
                }
            }
        }
    }
    directives.extend(handles.into_iter());
    directives
}
