// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Fuses reduce operators with parent operators if possible.

use crate::TransformArgs;
use expr::{AggregateExpr, AggregateFunc, MirRelationExpr, MirScalarExpr};

/// Fuses reduce operators with parent operators if possible.
#[derive(Debug)]
pub struct Reduce;

impl crate::Transform for Reduce {
    fn transform(
        &self,
        relation: &mut MirRelationExpr,
        _: TransformArgs,
    ) -> Result<(), crate::TransformError> {
        relation.visit_mut_pre(&mut |e| {
            self.action(e);
        });
        Ok(())
    }
}

impl Reduce {
    /// Fuses reduce operators with parent operators if possible.
    pub fn action(&self, relation: &mut MirRelationExpr) {
        if let MirRelationExpr::Reduce {
            input,
            group_key,
            aggregates,
            monotonic: _,
            expected_group_size: _,
        } = relation
        {
            if let MirRelationExpr::Reduce {
                input: inner_input,
                group_key: inner_group_key,
                aggregates: inner_aggregates,
                monotonic: _,
                expected_group_size: _,
            } = &mut **input
            {
                // Do nothing if outer key is not a subset of inner key
                if !group_key
                    .iter()
                    .all(|e| matches!(e, MirScalarExpr::Column(_)) && inner_group_key.contains(e))
                {
                    return;
                }

                if aggregates.is_empty() && inner_aggregates.is_empty() {
                    // Replace inner reduce with map + project (no grouping)
                    let mut outputs = vec![];
                    let mut scalars = vec![];
                    for e in inner_group_key {
                        if let MirScalarExpr::Column(i) = e {
                            outputs.push(*i);
                        } else {
                            scalars.push(e.clone());
                        }
                    }

                    let arity = inner_input.arity();
                    for i in 0..scalars.len() {
                        outputs.push(arity + i);
                    }

                    **input = inner_input.take_dangerous().map(scalars).project(outputs);
                } else if aggregates.len() == 1 && inner_aggregates.len() == 1 {
                    // TODO: This can be more general than just len() == 1
                    // Drop inner reduce and rewrite outer reduce if possible
                    if let AggregateExpr {
                        func: AggregateFunc::Count,
                        expr,
                        distinct: _,
                    } = inner_aggregates.first().unwrap()
                    {
                        if inner_group_key.contains(expr) {
                            return;
                        }

                        if let AggregateExpr {
                            func: AggregateFunc::SumInt64,
                            expr: MirScalarExpr::Column(i),
                            distinct: _,
                        } = aggregates.first().unwrap()
                        {
                            if *i != inner_group_key.len() {
                                return;
                            }

                            aggregates[0] = inner_aggregates[0].clone();
                            **input = inner_input.take_dangerous();

                            let mut outputs = Vec::with_capacity(group_key.len());
                            for i in 0..group_key.len() {
                                outputs.push(i);
                            }
                            outputs.push(group_key.len() + 1);

                            let agg_column = MirScalarExpr::Column(group_key.len());
                            *relation = relation
                                .take_dangerous()
                                .map(vec![MirScalarExpr::CallUnary {
                                    func: expr::UnaryFunc::CastInt64ToDecimal,
                                    expr: Box::new(agg_column),
                                }])
                                .project(outputs);
                        }
                    }
                }
            }
        }
    }
}
