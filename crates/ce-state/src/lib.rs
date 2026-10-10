//! CEngine state machines (draft v1). Pure and deterministic: given a validated manifest and
//! a current state, decide whether a move or an action is allowed and what it would do.
//! Nothing here touches storage; the caller applies the result inside one transaction.
//!
//! State codes follow `spec/catalogs/state-registry.json`: 3 digits, class by hundreds
//! (1xx pending, 2xx active, 3xx limited, 4xx failed, 5xx winding down, 6xx terminal).

use ce_manifest::Manifest;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    UnknownEntity(String),
    NoStateModel(String),
    UnknownState(String),
    UnknownAction(String),
    /// The move is not in the entity's state graph.
    InvalidTransition {
        from: String,
        to: String,
        allowed: Vec<String>,
    },
    /// The action cannot run from the current state.
    WrongState {
        action: String,
        current: String,
        required: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateClass {
    Pending,
    Active,
    Limited,
    Failed,
    WindingDown,
    Terminal,
}

impl StateClass {
    pub fn of(code: u16) -> Option<StateClass> {
        Some(match code / 100 {
            1 => StateClass::Pending,
            2 => StateClass::Active,
            3 => StateClass::Limited,
            4 => StateClass::Failed,
            5 => StateClass::WindingDown,
            6 => StateClass::Terminal,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub from: String,
    pub from_code: u16,
    pub to: String,
    pub to_code: u16,
}

/// The state graph of one entity.
pub struct Machine<'a> {
    entity: String,
    states: &'a serde_json::Map<String, Value>,
    initial: String,
}

impl<'a> Machine<'a> {
    pub fn new(m: &'a Manifest, entity: &str) -> Result<Machine<'a>, StateError> {
        let e = m.value["entities"]
            .get(entity)
            .ok_or_else(|| StateError::UnknownEntity(entity.to_string()))?;
        let model = e
            .get("state")
            .filter(|s| s.is_object())
            .ok_or_else(|| StateError::NoStateModel(entity.to_string()))?;
        Ok(Machine {
            entity: entity.to_string(),
            states: model["states"]
                .as_object()
                .ok_or_else(|| StateError::NoStateModel(entity.to_string()))?,
            initial: model["initial"].as_str().unwrap_or_default().to_string(),
        })
    }

    pub fn entity(&self) -> &str {
        &self.entity
    }

    pub fn initial(&self) -> &str {
        &self.initial
    }

    pub fn code(&self, state: &str) -> Result<u16, StateError> {
        self.states
            .get(state)
            .and_then(|s| s["code"].as_u64())
            .map(|c| c as u16)
            .ok_or_else(|| StateError::UnknownState(state.to_string()))
    }

    /// States reachable in one move, in manifest order.
    pub fn allowed_from(&self, state: &str) -> Result<Vec<String>, StateError> {
        let s = self
            .states
            .get(state)
            .ok_or_else(|| StateError::UnknownState(state.to_string()))?;
        Ok(s["to"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// True when no move leaves this state.
    pub fn is_terminal(&self, state: &str) -> Result<bool, StateError> {
        Ok(self.allowed_from(state)?.is_empty())
    }

    pub fn class(&self, state: &str) -> Result<Option<StateClass>, StateError> {
        Ok(StateClass::of(self.code(state)?))
    }

    pub fn transition(&self, from: &str, to: &str) -> Result<Move, StateError> {
        let from_code = self.code(from)?;
        let to_code = self.code(to)?;
        let allowed = self.allowed_from(from)?;
        if !allowed.iter().any(|a| a == to) {
            return Err(StateError::InvalidTransition {
                from: from.into(),
                to: to.into(),
                allowed,
            });
        }
        Ok(Move {
            from: from.into(),
            from_code,
            to: to.into(),
            to_code,
        })
    }
}

/// What running an action would do, already checked against the state graph.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionPlan {
    pub action: String,
    pub entity: String,
    /// Moves in effect order; each starts where the previous one ended.
    pub moves: Vec<Move>,
    /// `(field, expression)` pairs from `set` effects, in order. Expressions are not evaluated here.
    pub sets: Vec<(String, String)>,
    pub emits: Vec<String>,
    pub creates: Vec<Value>,
    pub is_async: bool,
}

/// Checks `requires_state` and walks the action's effects, validating every transition
/// from the state the previous effect left behind.
pub fn plan_action(m: &Manifest, action: &str, current: &str) -> Result<ActionPlan, StateError> {
    let a = m.value["actions"]
        .get(action)
        .ok_or_else(|| StateError::UnknownAction(action.to_string()))?;
    let entity = a["entity"].as_str().unwrap_or_default();
    let machine = Machine::new(m, entity)?;
    machine.code(current)?;

    if let Some(req) = a["requires_state"].as_array() {
        let required: Vec<String> = req
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        if !required.is_empty() && !required.iter().any(|r| r == current) {
            return Err(StateError::WrongState {
                action: action.into(),
                current: current.into(),
                required,
            });
        }
    }

    let mut plan = ActionPlan {
        action: action.into(),
        entity: entity.into(),
        moves: vec![],
        sets: vec![],
        emits: vec![],
        creates: vec![],
        is_async: a["async"].as_bool().unwrap_or(false),
    };
    let mut at = current.to_string();
    for effect in a["effect"].as_array().into_iter().flatten() {
        if let Some(set) = effect["set"].as_object() {
            for (k, v) in set {
                plan.sets
                    .push((k.clone(), v.as_str().unwrap_or_default().to_string()));
            }
        }
        if let Some(to) = effect["transition"].as_str() {
            let mv = machine.transition(&at, to)?;
            at = mv.to.clone();
            plan.moves.push(mv);
        }
        if let Some(c) = effect.get("create").filter(|c| c.is_object()) {
            plan.creates.push(c.clone());
        }
        if let Some(ev) = effect["emit"].as_str() {
            plan.emits.push(ev.to_string());
        }
    }
    Ok(plan)
}
