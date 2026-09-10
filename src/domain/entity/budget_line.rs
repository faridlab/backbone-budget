use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;
use rust_decimal::Decimal;
use super::AuditMetadata;

/// Strongly-typed ID for BudgetLine
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BudgetLineId(pub Uuid);

impl BudgetLineId {
    pub fn new(id: Uuid) -> Self { Self(id) }
    pub fn generate() -> Self { Self(Uuid::new_v4()) }
    pub fn into_inner(self) -> Uuid { self.0 }
}

impl std::fmt::Display for BudgetLineId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for BudgetLineId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl From<Uuid> for BudgetLineId {
    fn from(id: Uuid) -> Self { Self(id) }
}

impl From<BudgetLineId> for Uuid {
    fn from(id: BudgetLineId) -> Self { id.0 }
}

impl AsRef<Uuid> for BudgetLineId {
    fn as_ref(&self) -> &Uuid { &self.0 }
}

impl std::ops::Deref for BudgetLineId {
    type Target = Uuid;
    fn deref(&self) -> &Self::Target { &self.0 }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct BudgetLine {
    pub id: Uuid,
    pub budget_id: Uuid,
    pub account_id: Uuid,
    pub cost_center_id: Option<Uuid>,
    pub fiscal_period_id: Uuid,
    pub fiscal_year: i32,
    pub fiscal_month: Option<i32>,
    pub planned_amount: Decimal,
    pub notes: Option<String>,
    #[serde(default)]
    #[sqlx(json)]
    pub metadata: AuditMetadata,
}

impl BudgetLine {
    /// Create a builder for BudgetLine
    pub fn builder() -> BudgetLineBuilder {
        <BudgetLineBuilder as Default>::default()
    }

    /// Create a new BudgetLine with required fields
    pub fn new(budget_id: Uuid, account_id: Uuid, fiscal_period_id: Uuid, fiscal_year: i32, planned_amount: Decimal) -> Self {
        Self {
            id: Uuid::new_v4(),
            budget_id,
            account_id,
            cost_center_id: None,
            fiscal_period_id,
            fiscal_year,
            fiscal_month: None,
            planned_amount,
            notes: None,
            metadata: AuditMetadata::default(),
        }
    }

    /// Get the entity's unique identifier
    pub fn id(&self) -> &Uuid {
        &self.id
    }

    /// Get a strongly-typed ID for this entity
    pub fn typed_id(&self) -> BudgetLineId {
        BudgetLineId(self.id)
    }

    /// Get when this entity was created
    pub fn created_at(&self) -> Option<&DateTime<Utc>> {
        self.metadata.created_at.as_ref()
    }

    /// Get when this entity was last updated
    pub fn updated_at(&self) -> Option<&DateTime<Utc>> {
        self.metadata.updated_at.as_ref()
    }

    /// Check if this entity is soft deleted
    pub fn is_deleted(&self) -> bool {
        self.metadata.deleted_at.is_some()
    }

    /// Check if this entity is active (not deleted)
    pub fn is_active(&self) -> bool {
        self.metadata.deleted_at.is_none()
    }

    /// Get when this entity was deleted
    pub fn deleted_at(&self) -> Option<&DateTime<Utc>> {
        self.metadata.deleted_at.as_ref()
    }

    /// Get who created this entity
    pub fn created_by(&self) -> Option<&Uuid> {
        self.metadata.created_by.as_ref()
    }

    /// Get who last updated this entity
    pub fn updated_by(&self) -> Option<&Uuid> {
        self.metadata.updated_by.as_ref()
    }

    /// Get who deleted this entity
    pub fn deleted_by(&self) -> Option<&Uuid> {
        self.metadata.deleted_by.as_ref()
    }


    // ==========================================================
    // Fluent Setters (with_* for optional fields)
    // ==========================================================

    /// Set the cost_center_id field (chainable)
    pub fn with_cost_center_id(mut self, value: Uuid) -> Self {
        self.cost_center_id = Some(value);
        self
    }

    /// Set the fiscal_month field (chainable)
    pub fn with_fiscal_month(mut self, value: i32) -> Self {
        self.fiscal_month = Some(value);
        self
    }

    /// Set the notes field (chainable)
    pub fn with_notes(mut self, value: String) -> Self {
        self.notes = Some(value);
        self
    }

    // ==========================================================
    // Partial Update
    // ==========================================================

    /// Apply partial updates from a map of field name to JSON value
    pub fn apply_patch(&mut self, fields: std::collections::HashMap<String, serde_json::Value>) {
        for (key, value) in fields {
            match key.as_str() {
                "budget_id" => {
                    if let Ok(v) = serde_json::from_value(value) { self.budget_id = v; }
                }
                "account_id" => {
                    if let Ok(v) = serde_json::from_value(value) { self.account_id = v; }
                }
                "cost_center_id" => {
                    if let Ok(v) = serde_json::from_value(value) { self.cost_center_id = v; }
                }
                "fiscal_period_id" => {
                    if let Ok(v) = serde_json::from_value(value) { self.fiscal_period_id = v; }
                }
                "fiscal_year" => {
                    if let Ok(v) = serde_json::from_value(value) { self.fiscal_year = v; }
                }
                "fiscal_month" => {
                    if let Ok(v) = serde_json::from_value(value) { self.fiscal_month = v; }
                }
                "planned_amount" => {
                    if let Ok(v) = serde_json::from_value(value) { self.planned_amount = v; }
                }
                "notes" => {
                    if let Ok(v) = serde_json::from_value(value) { self.notes = v; }
                }
                _ => {} // ignore unknown fields
            }
        }
    }

    // <<< CUSTOM METHODS START >>>
    // <<< CUSTOM METHODS END >>>
}

impl super::Entity for BudgetLine {
    type Id = Uuid;

    fn entity_id(&self) -> &Self::Id {
        &self.id
    }

    fn entity_type() -> &'static str {
        "BudgetLine"
    }
}

impl backbone_core::PersistentEntity for BudgetLine {
    fn entity_id(&self) -> String {
        self.id.to_string()
    }
    fn set_entity_id(&mut self, id: String) {
        if let Ok(uuid) = uuid::Uuid::parse_str(&id) {
            self.id = uuid;
        }
    }
    fn created_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.metadata.created_at
    }
    fn set_created_at(&mut self, ts: chrono::DateTime<chrono::Utc>) {
        self.metadata.created_at = Some(ts);
    }
    fn updated_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.metadata.updated_at
    }
    fn set_updated_at(&mut self, ts: chrono::DateTime<chrono::Utc>) {
        self.metadata.updated_at = Some(ts);
    }
    fn deleted_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.metadata.deleted_at
    }
    fn set_deleted_at(&mut self, ts: Option<chrono::DateTime<chrono::Utc>>) {
        self.metadata.deleted_at = ts;
    }
}

impl backbone_orm::EntityRepoMeta for BudgetLine {
    fn column_types() -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        m.insert("id".to_string(), "uuid".to_string());
        m.insert("budget_id".to_string(), "uuid".to_string());
        m.insert("account_id".to_string(), "uuid".to_string());
        m.insert("cost_center_id".to_string(), "uuid".to_string());
        m.insert("fiscal_period_id".to_string(), "uuid".to_string());
        m
    }
    fn search_fields() -> &'static [&'static str] {
        &[]
    }
}

/// Builder for BudgetLine entity
///
/// Provides a fluent API for constructing BudgetLine instances.
/// System fields (id, metadata, timestamps) are auto-initialized.
#[derive(Debug, Clone, Default)]
pub struct BudgetLineBuilder {
    budget_id: Option<Uuid>,
    account_id: Option<Uuid>,
    cost_center_id: Option<Uuid>,
    fiscal_period_id: Option<Uuid>,
    fiscal_year: Option<i32>,
    fiscal_month: Option<i32>,
    planned_amount: Option<Decimal>,
    notes: Option<String>,
}

impl BudgetLineBuilder {
    /// Set the budget_id field (required)
    pub fn budget_id(mut self, value: Uuid) -> Self {
        self.budget_id = Some(value);
        self
    }

    /// Set the account_id field (required)
    pub fn account_id(mut self, value: Uuid) -> Self {
        self.account_id = Some(value);
        self
    }

    /// Set the cost_center_id field (optional)
    pub fn cost_center_id(mut self, value: Uuid) -> Self {
        self.cost_center_id = Some(value);
        self
    }

    /// Set the fiscal_period_id field (required)
    pub fn fiscal_period_id(mut self, value: Uuid) -> Self {
        self.fiscal_period_id = Some(value);
        self
    }

    /// Set the fiscal_year field (required)
    pub fn fiscal_year(mut self, value: i32) -> Self {
        self.fiscal_year = Some(value);
        self
    }

    /// Set the fiscal_month field (optional)
    pub fn fiscal_month(mut self, value: i32) -> Self {
        self.fiscal_month = Some(value);
        self
    }

    /// Set the planned_amount field (required)
    pub fn planned_amount(mut self, value: Decimal) -> Self {
        self.planned_amount = Some(value);
        self
    }

    /// Set the notes field (optional)
    pub fn notes(mut self, value: String) -> Self {
        self.notes = Some(value);
        self
    }

    /// Build the BudgetLine entity
    ///
    /// Returns Err if any required field without a default is missing.
    pub fn build(self) -> Result<BudgetLine, String> {
        let budget_id = self.budget_id.ok_or_else(|| "budget_id is required".to_string())?;
        let account_id = self.account_id.ok_or_else(|| "account_id is required".to_string())?;
        let fiscal_period_id = self.fiscal_period_id.ok_or_else(|| "fiscal_period_id is required".to_string())?;
        let fiscal_year = self.fiscal_year.ok_or_else(|| "fiscal_year is required".to_string())?;
        let planned_amount = self.planned_amount.ok_or_else(|| "planned_amount is required".to_string())?;

        Ok(BudgetLine {
            id: Uuid::new_v4(),
            budget_id,
            account_id,
            cost_center_id: self.cost_center_id,
            fiscal_period_id,
            fiscal_year,
            fiscal_month: self.fiscal_month,
            planned_amount,
            notes: self.notes,
            metadata: AuditMetadata::default(),
        })
    }
}
