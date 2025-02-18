// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use alloc::{format, vec::Vec};
use sql_parse::{issue_todo, InsertReplace, InsertReplaceFlag, InsertReplaceType, Issue, Spanned};

use crate::{
    type_expression::{type_expression, ExpressionFlags},
    type_select::{type_select, type_select_exprs, SelectType},
    typer::{typer_stack, ReferenceType, Typer},
    BaseType, SelectTypeColumn, Type,
};

/// Does the insert yield an auto increment id
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoIncrementId {
    Yes,
    No,
    Optional,
}

pub(crate) fn type_insert_replace<'a, 'b>(
    typer: &mut Typer<'a, 'b>,
    ior: &InsertReplace<'a>,
) -> (AutoIncrementId, Option<SelectType<'a>>) {
    let table = &ior.table;
    let columns = &ior.columns;

    if let Some(v) = table.get(1..) {
        for t in v {
            typer.issues.push(issue_todo!(t));
        }
    }

    let t = &table[0];
    let (s, auto_increment) = if let Some(schema) = typer.schemas.schemas.get(t.value) {
        if schema.view {
            typer
                .issues
                .push(Issue::err("Inserts into views not yet implemented", t));
        }
        let mut col_types = Vec::new();

        for col in columns {
            if let Some(schema_col) = schema.get_column(col.value) {
                col_types.push((schema_col.type_.ref_clone(), col.span()));
            } else {
                typer
                    .issues
                    .push(Issue::err("No such column in schema", col));
            }
        }
        (
            Some(col_types),
            schema.columns.iter().any(|c| c.auto_increment),
        )
    } else {
        typer.issues.push(Issue::err("Unknown table", t));
        (None, false)
    };

    if let Some(values) = &ior.values {
        for row in &values.1 {
            for (j, e) in row.iter().enumerate() {
                if let Some((et, ets)) = s.as_ref().and_then(|v| v.get(j)) {
                    let t = type_expression(typer, e, ExpressionFlags::default(), et.base());
                    if typer.matched_type(&t, et).is_none() {
                        typer.issues.push(
                            Issue::err(format!("Got type {}", t.t), e)
                                .frag(format!("Expected {}", et.t), ets),
                        );
                    } else if let Type::Args(_, args) = &t.t {
                        for (idx, arg_type, _) in args {
                            typer.constrain_arg(*idx, arg_type, et);
                        }
                    }
                } else {
                    type_expression(typer, e, ExpressionFlags::default(), BaseType::Any);
                }
            }
        }
    }

    if let Some(select) = &ior.select {
        let select = type_select(typer, select, true);
        if let Some(s) = s {
            for i in 0..usize::max(s.len(), select.columns.len()) {
                match (s.get(i), select.columns.get(i)) {
                    (Some((et, ets)), Some(t)) => {
                        if typer.matched_type(&t.type_, et).is_none() {
                            typer.issues.push(
                                Issue::err(format!("Got type {}", t.type_.t), &t.span)
                                    .frag(format!("Expected {}", et.t), ets),
                            );
                        }
                    }
                    (None, Some(t)) => {
                        typer
                            .issues
                            .push(Issue::err("Column in select not in insert", &t.span));
                    }
                    (Some((_, ets)), None) => {
                        typer
                            .issues
                            .push(Issue::err("Missing column in select", ets));
                    }
                    (None, None) => {
                        panic!("ICE")
                    }
                }
            }
        }
    }

    let mut guard = typer_stack(
        typer,
        |t| core::mem::take(&mut t.reference_types),
        |t, v| t.reference_types = v,
    );
    let typer = &mut guard.typer;

    if let Some(s) = typer.schemas.schemas.get(t.value) {
        let mut columns = Vec::new();
        for c in &s.columns {
            columns.push((c.identifier, c.type_.ref_clone()));
        }
        for v in &typer.reference_types {
            if v.name == Some(t.value) {
                typer.issues.push(
                    Issue::err("Duplicate definitions", t).frag("Already defined here", &v.span),
                );
            }
        }
        typer.reference_types.push(ReferenceType {
            name: Some(t.value),
            span: t.span(),
            columns,
        });
    }

    if let Some((_, set)) = &ior.set {
        for (key, _, value) in set {
            let mut cnt = 0;
            let mut t = None;
            for r in &typer.reference_types {
                for c in &r.columns {
                    if c.0 == key.value {
                        cnt += 1;
                        t = Some(c.clone());
                    }
                }
            }
            if cnt > 1 {
                type_expression(typer, value, ExpressionFlags::default(), BaseType::Any);
                let mut issue = Issue::err("Ambiguous reference", key);
                for r in &typer.reference_types {
                    for c in &r.columns {
                        if c.0 == key.value {
                            issue = issue.frag("Defined here", &r.span);
                        }
                    }
                }
                typer.issues.push(issue);
            } else if let Some(t) = t {
                let value_type =
                    type_expression(typer, value, ExpressionFlags::default(), t.1.base());
                if typer.matched_type(&value_type, &t.1).is_none() {
                    typer.issues.push(Issue::err(
                        format!("Got type {} expected {}", value_type, t.1),
                        value,
                    ));
                } else if let Type::Args(_, args) = &value_type.t {
                    for (idx, arg_type, _) in args {
                        typer.constrain_arg(*idx, arg_type, &t.1);
                    }
                }
            } else {
                type_expression(typer, value, ExpressionFlags::default(), BaseType::Any);
                typer.issues.push(Issue::err("Unknown identifier", key));
            }
        }
    }

    if let Some((_, update)) = &ior.on_duplicate_key_update {
        for (key, _, value) in update {
            let mut cnt = 0;
            let mut t = None;
            for r in &typer.reference_types {
                for c in &r.columns {
                    if c.0 == key.value {
                        cnt += 1;
                        t = Some(c.clone());
                    }
                }
            }
            let flags = ExpressionFlags::default().with_in_on_duplicate_key_update(true);
            if cnt > 1 {
                type_expression(typer, value, flags, BaseType::Any);
                let mut issue = Issue::err("Ambiguous reference", key);
                for r in &typer.reference_types {
                    for c in &r.columns {
                        if c.0 == key.value {
                            issue = issue.frag("Defined here", &r.span);
                        }
                    }
                }
                typer.issues.push(issue);
            } else if let Some(t) = t {
                let value_type = type_expression(typer, value, flags, t.1.base());
                if typer.matched_type(&value_type, &t.1).is_none() {
                    typer.issues.push(Issue::err(
                        format!("Got type {} expected {}", value_type, t.1),
                        value,
                    ));
                } else if let Type::Args(_, args) = &value_type.t {
                    for (idx, arg_type, _) in args {
                        typer.constrain_arg(*idx, arg_type, &t.1);
                    }
                }
            } else {
                type_expression(typer, value, flags, BaseType::Any);
                typer.issues.push(Issue::err("Unknown identifier", key));
            }
        }
    }

    if let Some(on_conflict) = &ior.on_conflict {
        match &on_conflict.target {
            sql_parse::OnConflictTarget::Column { name } => {
                let mut t = None;
                for r in &typer.reference_types {
                    for c in &r.columns {
                        if c.0 == name.value {
                            t = Some(c.clone());
                        }
                    }
                }
                if t.is_none() {
                    typer.issues.push(Issue::err("Unknown identifier", name));
                }
                //TODO check if there is a unique constraint on column
            }
            sql_parse::OnConflictTarget::OnConstraint {
                on_constraint_span, ..
            } => {
                typer.issues.push(issue_todo!(on_constraint_span));
            }
            sql_parse::OnConflictTarget::None => (),
        }

        match &on_conflict.action {
            sql_parse::OnConflictAction::DoNothing(_) => (),
            sql_parse::OnConflictAction::DoUpdateSet { sets, where_, .. } => {
                for (key, value) in sets {
                    let mut cnt = 0;
                    let mut t = None;
                    for r in &typer.reference_types {
                        for c in &r.columns {
                            if c.0 == key.value {
                                cnt += 1;
                                t = Some(c.clone());
                            }
                        }
                    }
                    let flags = ExpressionFlags::default().with_in_on_duplicate_key_update(true);
                    if cnt > 1 {
                        type_expression(typer, value, flags, BaseType::Any);
                        let mut issue = Issue::err("Ambiguous reference", key);
                        for r in &typer.reference_types {
                            for c in &r.columns {
                                if c.0 == key.value {
                                    issue = issue.frag("Defined here", &r.span);
                                }
                            }
                        }
                        typer.issues.push(issue);
                    } else if let Some(t) = t {
                        let value_type = type_expression(typer, value, flags, t.1.base());
                        if typer.matched_type(&value_type, &t.1).is_none() {
                            typer.issues.push(Issue::err(
                                format!("Got type {} expected {}", value_type, t.1),
                                value,
                            ));
                        } else if let Type::Args(_, args) = &value_type.t {
                            for (idx, arg_type, _) in args {
                                typer.constrain_arg(*idx, arg_type, &t.1);
                            }
                        }
                    } else {
                        type_expression(typer, value, flags, BaseType::Any);
                        typer.issues.push(Issue::err("Unknown identifier", key));
                    }
                }
                if let Some((_, where_)) = where_ {
                    type_expression(typer, where_, ExpressionFlags::default(), BaseType::Bool);
                }
            }
        }
    }

    let returning_select = match &ior.returning {
        Some((returning_span, returning_exprs)) => {
            let columns = type_select_exprs(typer, returning_exprs, true)
                .into_iter()
                .map(|(name, type_, span)| SelectTypeColumn { name, type_, span })
                .collect();
            Some(SelectType {
                columns,
                select_span: returning_span.join_span(returning_exprs),
            })
        }
        None => None,
    };

    core::mem::drop(guard);

    let auto_increment_id = if auto_increment && matches!(ior.type_, InsertReplaceType::Insert(_)) {
        if ior
            .flags
            .iter()
            .any(|f| matches!(f, InsertReplaceFlag::Ignore(_)))
            || ior.on_duplicate_key_update.is_some()
        {
            AutoIncrementId::Optional
        } else {
            AutoIncrementId::Yes
        }
    } else {
        AutoIncrementId::No
    };

    (auto_increment_id, returning_select)
}
