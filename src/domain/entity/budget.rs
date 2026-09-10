use chrono::{DateTime, Utc, NaiveDate};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use super::BudgetStatus;
use super::BudgetEnforcement;
use super::AuditMetadata;

/// Strongly-typed ID for Budget
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BudgetId(pub Uuid);

impl BudgetId {
    pub fn new(id: Uuid) -> Self { Self(id) }
    pub fn generate() -> Self { Self(Uuid::new_v4()) }
    pub fn into_inner(self) -> Uuid { self.0 }
}

impl std::fmt::Display for BudgetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for BudgetId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl From<Uuid> for BudgetId {
    fn from(id: Uuid) -> Self { Self(id) }
}

impl From<BudgetId> for Uuid {
    fn from(id: BudgetId) -> Self { id.0 }
}

impl AsRef<Uuid> for BudgetId {
    fn as_ref(&self) -> &Uuid { &self.0 }
}

impl std::ops::Deref for BudgetId {
    type Target = Uuid;
    fn deref(&self) -> &Self::Target { &self.0 }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Budget {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub fiscal_year: i32,
    pub date_from: NaiveDate,
    pub date_to: NaiveDate,
    pub status: BudgetStatus,
    pub enforcement: BudgetEnforcement,
    #[serde(default)]
    #[sqlx(json)]
    pub metadata: AuditMetadata,
}

impl Budget {
    /// Create a builder for Budget
    pub fn builder() -> BudgetBuilder {
        <BudgetBuilder as Default>::default()
    }

    /// Create a new Budget with required fields
    pub fn new(code: String, name: String, fiscal_year: i32, date_from: NaiveDate, date_to: NaiveDate, status: BudgetStatus, enforcement: BudgetEnforcement) -> Self {
        Self {
            id: Uuid::new_v4(),
            code,
            name,
            description: None,
            fiscal_year,
            date_from,
            date_to,
            status,
            enforcement,
            metadata: AuditMetadata::default(),
        }
    }

    /// Get the entity's unique identifier
    pub fn id(&self) -> &Uuid {
        &self.id
    }

    /// Get a strongly-typed ID for this entity
    pub fn typed_id(&self) -> BudgetId {
        BudgetId(self.id)
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

    /// Get the current status
    pub fn status(&self) -> &BudgetStatus {
        &self.status
    }


    // ==========================================================
    // Fluent Setters (with_* for optional fields)
    // ==========================================================

    /// Set the description field (chainable)
    pub fn with_description(mut self, value: String) -> Self {
        self.description = Some(value);
        self
    }

    // ==========================================================
    // Partial Update
    // ==========================================================

    /// Apply partial updates from a map of field name to JSON value
    pub fn apply_patch(&mut self, fields: std::collections::HashMap<String, serde_json::Value>) {
        for (key, value) in fields {
            match key.as_str() {
                "code" => {
                    if let Ok(v) = serde_json::from_value(value) { self.code = v; }
                }
                "name" => {
                    if let Ok(v) = serde_json::from_value(value) { self.name = v; }
                }
                "description" => {
                    if let Ok(v) = serde_json::from_value(value) { self.description = v; }
                }
                "fiscal_year" => {
                    if let Ok(v) = serde_json::from_value(value) { self.fiscal_year = v; }
                }
                "date_from" => {
                    if let Ok(v) = serde_json::from_value(value) { self.date_from = v; }
                }
                "date_to" => {
                    if let Ok(v) = serde_json::from_value(value) { self.date_to = v; }
                }
                "status" => {
                    if let Ok(v) = serde_json::from_value(value) { self.status = v; }
                }
                "enforcement" => {
                    if let Ok(v) = serde_json::from_value(value) { self.enforcement = v; }
                }
                _ => {} // ignore unknown fields
            }
        }
    }

    // <<< CUSTOM METHODS START >>>
    // <<< CUSTOM METHODS END >>>
}

impl super::Entity for Budget {
    type Id = Uuid;

    fn entity_id(&self) -> &Self::Id {
        &self.id
    }

    fn entity_type() -> &'static str {
        "Budget"
    }
}

impl backbone_core::PersistentEntity for Budget {
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

impl backbone_orm::EntityRepoMeta for Budget {
    fn column_types() -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        m.insert("id".to_string(), "uuid".to_string());
        m.insert("status".to_string(), "budget_status".to_string());
        m.insert("enforcement".to_string(), "budget_enforcement".to_string());
        m
    }
    fn search_fields() -> &'static [&'static str] {
        &["code", "name"]
    }
}

/// Builder for Budget entity
///
/// Provides a fluent API for constructing Budget instances.
/// System fields (id, metadata, timestamps) are auto-initialized.
#[derive(Debug, Clone, Default)]
pub struct BudgetBuilder {
    code: Option<String>,
    name: Option<String>,
    description: Option<String>,
    fiscal_year: Option<i32>,
    date_from: Option<NaiveDate>,
    date_to: Option<NaiveDate>,
    status: Option<BudgetStatus>,
    enforcement: Option<BudgetEnforcement>,
}

impl BudgetBuilder {
    /// Set the code field (required)
    pub fn code(mut self, value: String) -> Self {
        self.code = Some(value);
        self
    }

    /// Set the name field (required)
    pub fn name(mut self, value: String) -> Self {
        self.name = Some(value);
        self
    }

    /// Set the description field (optional)
    pub fn description(mut self, value: String) -> Self {
        self.description = Some(value);
        self
    }

    /// Set the fiscal_year field (required)
    pub fn fiscal_year(mut self, value: i32) -> Self {
        self.fiscal_year = Some(value);
        self
    }

    /// Set the date_from field (required)
    pub fn date_from(mut self, value: NaiveDate) -> Self {
        self.date_from = Some(value);
        self
    }

    /// Set the date_to field (required)
    pub fn date_to(mut self, value: NaiveDate) -> Self {
        self.date_to = Some(value);
        self
    }

    /// Set the status field (required)
    pub fn status(mut self, value: BudgetStatus) -> Self {
        self.status = Some(value);
        self
    }

    /// Set the enforcement field (required)
    pub fn enforcement(mut self, value: BudgetEnforcement) -> Self {
        self.enforcement = Some(value);
        self
    }

    /// Build the Budget entity
    ///
    /// Returns Err if any required field without a default is missing.
    pub fn build(self) -> Result<Budget, String> {
        let code = self.code.ok_or_else(|| "code is required".to_string())?;
        let name = self.name.ok_or_else(|| "name is required".to_string())?;
        let fiscal_year = self.fiscal_year.ok_or_else(|| "fiscal_year is required".to_string())?;
        let date_from = self.date_from.ok_or_else(|| "date_from is required".to_string())?;
        let date_to = self.date_to.ok_or_else(|| "date_to is required".to_string())?;
        let status = self.status.ok_or_else(|| "status is required".to_string())?;
        let enforcement = self.enforcement.ok_or_else(|| "enforcement is required".to_string())?;

        Ok(Budget {
            id: Uuid::new_v4(),
            code,
            name,
            description: self.description,
            fiscal_year,
            date_from,
            date_to,
            status,
            enforcement,
            metadata: AuditMetadata::default(),
        })
    }
}
