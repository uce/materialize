# High level representation for SQL queries

## Summary

<!--
// Brief, high-level overview. A few sentences long.
// Be sure to capture the customer impact - framing this as a release note may be useful.
-->

The present docuemnt proposes the introduction of a new high-level representation for SQL queries for
replacing the current `HirRelationExpr` enum.

The proposal in this document is based on the Query Graph Model representation first introduced in
[H. Pirahesh et al](http://projectsweb.cs.washington.edu/research/projects/db/weld/pirahesh-starburst-92.pdf).

## Goals

<!--
// Enumerate the concrete goals that are in scope for the project.
-->

* The high level representation of SQL queries must:
    * represent the query at a conceptual level, free of syntactic concepts,
    * be a self-contained data structure,
    * be easy to use,
    * be normalization-friendly.
    * allow supporting complex features such as recursion in CTEs,
* Proper support of `LATERAL` joins (#6875)
* Support for functional dependency analysis during name resolution.

## Non-Goals

* List all the transformations that will be moved from `Mir` to `Hir`.

<!--
// Enumerate potential goals that are explicitly out of scope for the project
// ie. what could we do or what do we want to do in the future - but are not doing now
-->

## Description

<!--
// Describe the approach in detail. If there is no clear frontrunner, feel free to list all approaches in alternatives.
// If applicable, be sure to call out any new testing/validation that will be required
-->

In QGM, a query, as its name implies, is represented as a graph model. This graph will be represented
by a `Model` struct, that will be declated as follows:

```rust
    struct Model<'a> {
        top_box: BoxId,
        boxes: HashMap<BoxId, Box<QueryBox<'a>>>,
        quantifiers: HashMap<QuantifierId, Box<Quantifier>>,
    }
```

The graph has a top level box, which is the entry point of the query. In this proposal, all boxes and quantifiers
are owned by the model and are referenced by its unique identifier.

Boxes represent high-level conceptual operators, ie. they don't correspond to any execution strategy. Each box
has a set of input quantifiers, which describe the semantics of how the underlying boxes will be accessed.

The following snippet contains the definition of the different types of operators and quantifiers. Since both
all type of boxes and quantifiers have elements in common, both boxes and quantifiers are not directly
represented as enum, but as struct with an enum type, containing the per-type specific members.

```rust

    type QuantifierId = usize;
    type BoxId = usize;
    type QuantifierSet = BTreeSet<QuantifierId>;

    struct QueryBox<'a> {
        /// the model the box belongs to
        model: &'a Model<'a>,
        /// uniquely identifies the box within the model
        id: BoxId,
        /// the type of the box
        box_type: BoxType,
        /// the projection of the box
        columns: Vec<Column>,
        /// the input quantifiers of the box
        quantifiers: QuantifierSet,
        /// quantifiers ranging over this box
        ranging_quantifiers: QuantifierSet,
    }

    enum BoxType {
        BaseTable(BaseTable),
        Except,
        Grouping(Grouping),
        Intersect,
        OuterJoin(OuterJoin),
        Select(Select),
        TableFunction(TableFunction),
        Union,
        Values(Values),
    }

    struct Quantifier {
        /// uniquely identifiers the quantifier within the model
        id: QuantifierId,
        /// the type of the quantifier
        quantifier_type: QuantifierType,
        /// the input box of this quantifier
        input_box: BoxId,
        /// the box that owns this quantifier
        parent_box: BoxId,
        /// alias for name resolution purposes
        alias: Option<String>,
    }

    enum QuantifierType {
        All,
        Any,
        Existential,
        Foreach,
        PreservedForeach,
        Scalar,
    }
```

Note that input quantifiers are logically owned by a single box, but there may be several quantifiers ranging
over the same box. That is the case, for example, for base tables, views or CTEs, either explicit CTEs used in
the query or discovered via some query transformation.

As shown above, there aren't many different types of operators, since QGM is meant to be a representation for
query normalization. The core operator is represented by the `Select` box, which represents a whole query block
(sub-block).

```rust
    struct Select {
        predicates: Vec<Box<Expr>>,
        order_key: Option<Vec<Box<Expr>>>,
        limit: Option<Expr>,
        offset: Option<Expr>,
    }
```

There a few subtle constraints that are not explicit in the representation above:

* `BaseTable`, `TableFunction` and `Values` cannot have input quantifiers.
* `Union`, `Except` and `Intersect` can only have input quantifiers of type `Foreach`.
* Subquery quantifiers (`All`, `Any`, `Existential` and `Scalar`) are only allowed in `Select` boxes.
* `Grouping` must have a single input quantifier of type `Foreach` ranging over a `Select` box.
* A `Grouping` box is always ranged-over by a `Select` box.
* `OuterJoin` must have at leat an input quantifier of type `PreservedForeach`. The remaining onee, if any, must
  be of type `Foreach`. An `OuterJoin` with all `PreservedForeach` input quantifiers represents a `FULL OUTER JOIN`.
  Note: temporarily during the generation of the query model we could allow subquery quantifiers in `OuterJoin` boxes for
  subqueries in the `ON`-clause of the outer join, but should push down the subquery to the non-preserving side.
  Note 2: In QGM there is no distiction between a `LEFT JOIN` and a `RIGHT JOIN`, since that's a concept that belongs
  only in the AST.

Some of the constraints above are just conventions for making query transformation easier due to having to cover
fewer cases. The rest are just constructions that don't make sense semantically speaking.

All boxes have an ordered projection, represented as a vector of columns, defined as:

```rust
    struct Column {
        expr: Expr,
        alias: Option<String>,
    }
```

### Notes on expression representation

Column have two representations in QGM: base columns and column references. Base columns are only allowed in expressions
contained in data source operators, specifically in the projection of boxes of type `BaseTable` and `TableFunction`.

```rust
    enum Expr {
        // ...
        ColumnReference(ColumnReference),
        BaseColumn(usize),
    }

    struct ColumnReference {
        quantifier_id: QuantifierId,
        position: usize,
    }
```

`ColumnReference` is used everywhere else. A `ColumnReference` may either reference a quantifier of the same
box that owns the containing expression or a quantifier from some parent box.

The underlying expression behind a column reference can be obtained via a `dereference` method, whose implementation
could be as follows:

```rust
    impl ColumnReference {
        fn dereference<'a>(&self, model: &'a Model) -> &'a Expr {
            let input_box = model
                .quantifiers
                .get(&self.quantifier_id)
                .unwrap()
                .input_box;
            &model.boxes.get(&input_box).unwrap().columns[self.position].expr
        }
    }
```

Since this proposal uses identifiers instead of pointers, most methods in the implementation `Expr` will need to
receive a reference to the model as a parameter. For example, a method for determining whether an expression is
nullable or not may need to dereference a column reference, for which it needs acces to the model:

```rust
    impl Expr {
        fn nullable(&self, model: &Model) -> bool {
            match self {
                ...
                Expr::ColumnReference(c) => c.dereference(model).nullable(model),
            }
        }
    }
```

### Examples

This section includes examples of how some queries look like in QGM. This visual representation will be generated
from the representation decribed in the previous section. Note that having a visual way of representing the query
is very helpful during query transformation development/troubleshooting.

In this visual representation, predicates referencing columns from 1 or 2 quantifiers are represented as edges
connecting the quantifiers involved in the predicate.

#### Simple `SELECT *`

![Simple SELECT *](qgm/simple-select-star.svg)

#### Simple `GROUP BY`

![Simple GROUP BY](qgm/simple-group-by.svg)


#### `GROUP BY + HAVING`

![GROUP BY + HAVING](qgm/simple-group-by-having.svg)

Note that the having filter is just a regular predicate on the `Select` box ranging over the `Grouping` box.

#### Comma join

![Simple comma join](qgm/simple-comma-join.svg)

#### Inner join

![Simple inner join](qgm/simple-inner-join.svg)

Note that the inner join above is semantically equivalent to the comma join in the previous example. Boxes 1 and 2
represent the binary inner joins in the query, but they can be squashed into box 0, without altering the results of
the query. In fact, the normalization step will simplify this query leaving it exactly as the one in the example above:

![Simple inner join after normalization](qgm/simple-inner-join-after-normalization.svg)

#### Outer join

![Simple left join](qgm/simple-left-join.svg)

![Simple right join](qgm/simple-right-join.svg)

Note that in QGM there is no join direction, so left and right joins have the same exact representation. Only the type
of the quantifiers change its order.

@todo ramble about outer join as a special correlated operand and alternate representations after normalization.

#### Cross join

A `CROSS JOIN` can be represented as a `Select` box with no predicates as shown below:

![Simple cross join](qgm/simple-cross-join.svg)

#### CTEs

![Simple CTE](qgm/simple-cte.svg)

Quantifiers 2 and 3 are ranging over the same box, which represents the CTE. Box 2 doesn't alter the results of
box 0, but just adds aliases for the columns, for name resolution purposes. Normalization will get rid of all
the intermediate `Select` boxes, leaving the query as follows:

![Simple CTE after normalization](qgm/simple-cte-after-normalization.svg)

#### Lateral joins

A `LATERAL` join is just a join where one of its operands is correlated with the remaining ones, ie. a sub-graph
containing column references from quantifiers belonging in the parent context. For instance, in the following
example quantifier 4 is correlated within box 0, since its sub-graph references a column from quantifier 0 which
belongs in box 0. This correlation is made explicit by the edge going from Q1 in box 2 to Q0 in box 0.

![Lateral join](qgm/lateral-join.svg)

We will see later how we could decorrelate a query like that via transformations of the query model.

#### `NATURAL` joins

`NATURAL` joins don't have an explicit representation in QGM since, like `LATERAL`, it is a name resolution concept
that doesn't make sense anymore after it.

#### `EXISTS` and `IN SELECT`

`EXISTS` and `IN SELECT` subqueries are represented via `Existential` quantifiers. In fact, `EXISTS` subqueries
are represented as `1 IN (SELECT 1 FROM (<exists subquery>))` as shown in the second example below.

![IN SELECT](qgm/simple-in-select.svg)

![EXISTS](qgm/simple-exists.svg)

Given that the two queries above are equivalent, the normalization process should normalize both to the same
representation.

#### Scalar subqueries

#### `VALUES`

![VALUES](qgm/simple-values.svg)

![VALUES with alias](qgm/simple-values-with-alias.svg)

#### `UNION`

### Name resolution

As shown above, the query graph already contains almost all the information needed for name resoltion. Since the
query graph is built in a bottom-up manner, we can use the input quantifiers for resolving names within the
current part of the query being processed.

It is important to restate the constraint mentioned above: all column references in expressions within each box
*must* only point to quantifiers either within the same box or within an ancestor box through a chain of unique
children (correlation).

This section explains how the query graph model being built can be used for name resolution purposes using the following
query as an example:

![Name resolution](qgm/name-resolution-1.svg)

When planning the `WHERE` clause, belongs in box 0 as shown above, the resulting expression must only reference
quantifiers Q0 and Q5, however the relations visible in the scope according to the SQL standard are the relations
represented by Q0, Q1 and Q4, ie. the leaf quantifiers of the comma join (represented by box 0).

A new `NameResolutionContext` struct will encapsulate the name resolution logic, which resolves column names
against the leaf quantifiers but lifts the column references through the projection of the intermediate boxes,
all the way up to the current box.

Following with the example, when resolving the reference to `y` in the `WHERE` clause, we will find that among
the leaf quantifiers (Q0, Q1, Q4), only quantifier Q4 projects a column named `y`, so the column is resolved as
`Q4.c0`, ie. the first column amongst the columns projected by Q4's input box (box 4).
Since we must return a expression referencing only Q0 and Q5, we need to follow the linked chain made by
`Quantifier::parent_box` and `QueryBox::ranging_quantifiers` until we reach a quantifier ranged over by
box 0. The parent box of `Q4` is box 2, which projects `Q4.c0` as its forth column and it's ranged over by Q5.
Therefore, following that chain, we have resolved that `y` means `Q5.c3` within the context of box 0.

In the query above had and explicit projection or an ORDER BY clause names would be resolved against the same
leaf quantifiers and the resulting column references lifted following the same process.

Basically, a `NameResolutionContext` instance will represent the context of the `FROM` clause.

#### Name resolution within subqueries

Expressions within subqueries must see the symbols visible from the context the subquery is in. To support that
`NameResultionContext` will contain an optional reference to a parent `NameResolutionContext`, that is passed
down for planning the subquery, so that if a name cannot be resolved against the context of the `FROM` clause of
the subquery, we go through the chain of parent contexts until we find a symbol that matches in one of them.

#### Name resolution within the `ON` and `USING` clauses

In the following example, the binary `LEFT JOIN` is represented by box 2, and hence, the `ON` clause belongs in
that box.

![Name resolution within the `ON` clause](qgm/name-resolution-3.svg)

When planning a binary join, we will create a new `NameResolutionContext` which parent context is the same
as the parent context of the parent comma join. The `NameResolutionContext` for the comma join is the sibling
context, only visible by `LATERAL` join operands (more on that in the next subsection).

The leaf quantifiers for the binary `LEFT JOIN` in the example above are Q1 and Q4. Once we are done planning
the binary join, these leaf quantifiers are added as leaf quantifiers in the `NameResoltionContext` of the
parent join.

#### Name resolution within `LATERAL` joins

When planning a `LATERAL` join operand, the `NameResolutionContext` for the join the `LATERAL` operand
belongs to will be put temporarily in `lateral` mode, and passed down as the parent context of the query
within the `LATERAL` join operand. When in `lateral` mode, a `NameResolutionContext` tries to resolve
a name against its sibling context before it goes to its parent context.

#### Name resolution of `GROUP BY` queries

Symbols in the `GROUP BY` clause will be resolved as well against the `NameResoltionContext` representing
the scope exposed by the `FROM` clause, but then lifted through the projection of the `Select` box representing
the join that feeds the `Grouping` box created for the `GROUP BY` clause.

Symbols in the `HAVING` clause and in the projection of the `GROUP BY` are also resolved against the
`NameResoltionContext` of the `FROM` clause, but then lifted twice: once through the `Select` box representing
the join that feeds the `Grouping` box and then through the `Grouping` box itself (since the projection
of a `GROUP BY` and the predicates in the `HAVING` clause belong in `Select` box on top of the `Grouping` box).

Lifting expressions through a grouping box is a bit special:

* The projection of a `Grouping` box can only contain: columns references from the input quantifier that are
  present in the grouping key, references to columns from the input quantifier that functionally depend on a column
  in the grouping key, or aggregate expressions, which parameters must be column references from the input quantifier.
* Aggregate expressions are lifted as column references.
* Columns from the input quantifier that neither appear in the grouping nor functionally depend on any column in the
  grouping key, cannot be lifted and hence an error is returned.

@todo example

#### Name resolution of CTEs

A `NameResolutionContext` instance will be created for storing the processed CTEs.

#### Name resolution implementation

An example of implementation of the name resolution process described in this section can be seen
[here](https://github.com/asenac/rust-sql/blob/master/src/query_model/model_generator.rs#L19).
The code will be very similar, with the difference being that in that implementation boxes and quantifiers
are referenced using shared pointers, rather than identifiers.

```rust
struct NameResolutionContext<'a> {
    owner_box: Option<BoxId>,
    quantifiers: Vec<QuantifierId>,
    ctes: Option<HashMap<String, BoxId>>,
    parent_context: Option<&'a NameResolutionContext<'a>>,
    sibling_context: Option<&'a NameResolutionContext<'a>>,
    is_lateral: bool,
}
```

### Distinctness and unique keys

The `DISTINCT` handling in the QGM paper cite above is a bit messy, so the solution proposed here differs a bit
of the one described in that paper...

@todo to be continued

### Query model transformations: query normalization stage

Some normalization transformations are better/easier done with a representation at a higher level than our current
`MirRelationExpr` representation. Specially those around SQL-specific concepts such as outer joins that are
lost during lowering. Several examples of this are #6932, #6987 or #6988, but the list of unsupported cases that are
hard to support at the moment is much longer.

For example, consider the following two equivalent queries:

```
materialize=> explain select * from t1, lateral (select count(*) from t2 group by t1.f1);
                Optimized Plan                
----------------------------------------------
 %0 =                                        +
 | Get materialize.public.t1 (u254)          +
                                             +
 %1 =                                        +
 | Get materialize.public.t1 (u254)          +
 | Distinct group=(#0)                       +
 | ArrangeBy ()                              +
                                             +
 %2 =                                        +
 | Get materialize.public.t2 (u256)          +
                                             +
 %3 =                                        +
 | Join %1 %2                                +
 | | implementation = Differential %2 %1.()  +
 | | demand = (#0)                           +
 | Reduce group=(#0)                         +
 | | agg count(true)                         +
 | ArrangeBy (#0)                            +
                                             +
 %4 =                                        +
 | Join %0 %3 (= #0 #2)                      +
 | | implementation = Differential %0 %3.(#0)+
 | | demand = (#0, #1, #3)                   +
 | Project (#0, #1, #3)                      +
 
(1 row)
materialize=> explain select * from t1, (select count(*) from t2);
               Optimized Plan               
--------------------------------------------
 %0 = Let l0 =                             +
 | Get materialize.public.t2 (u256)        +
 | Reduce group=()                         +
 | | agg count(true)                       +
                                           +
 %1 =                                      +
 | Get materialize.public.t1 (u254)        +
                                           +
 %2 =                                      +
 | Get %0 (l0)                             +
 | Negate                                  +
 | Project ()                              +
                                           +
 %3 =                                      +
 | Constant ()                             +
                                           +
 %4 =                                      +
 | Union %2 %3                             +
 | Map 0                                   +
                                           +
 %5 =                                      +
 | Union %0 %4                             +
 | ArrangeBy ()                            +
                                           +
 %6 =                                      +
 | Join %1 %5                              +
 | | implementation = Differential %1 %5.()+
 | | demand = (#0..#2)                     +
 
(1 row)
materialize=>  
```

Ideally, any two semantically equivalent queries should result in the same execution plan: the most optimal one
for obtaining/computing the desired results. However, after lowering the first query above, we are not able to
detect that the grouping key is constant wrt the input of the aggregation and can then be removed, reducing the
complexity of the resulting dataflow as shown in the plan for the second query, where the transformation has been
manually applied.

With a small set of normalization transformations applied on the high level representation of the querey, before
lowering it, we could easily fix many cases like the ones listed above. These transformations could be used to
decorrelate some, if not all, cases where the query can be expressed with equivalent non-correlated SQL. The big
hammer used in `lowering.rs` will then be used for everything else that cannot be expressed with valid SQL in
a decorrelated manner (for example, a correlated lateral non-preserving side of an outer join cannot be decorrelated
in SQL since no predicate can cross the non-preserving boundary).

### Recursive CTEs support

A recursive CTE is a CTE with a union with two branches where one of the branches references the CTE and the other
one doesn't (representing the base case). This can be easily supported by the proposed implementation. Circular
memory ownership issues are avoided by making the model own all the nodes.

Any traveral of the query graph must keep a set/bitset of visited boxes since the same box can be ranged over by
several quantifiers. The same set/bitset will prevent infinite loops/stack overflows when traversing a recursive
query.

## Alternatives

<!--
// Similar to the Description section. List of alternative approaches considered, pros/cons or why they were not chosen
-->

* QGM with interior mutability, shared pointers and so on as implemented [here](https://github.com/asenac/rust-sql).
    * Making all boxes to be directly owned by the model makes recursion easier to support.
* Relational algebra representation
* Convert `MirRelationExpr` into a normalization-friendly representation with explicit `outer join` operator.

## Milestones

## Open questions

* Duplication in `transform` crate
* Panicking or not

<!--
// Anything currently unanswered that needs specific focus. This section may be expanded during the doc meeting as
// other unknowns are pointed out.
// These questions may be technical, product, or anything in-between.
-->
