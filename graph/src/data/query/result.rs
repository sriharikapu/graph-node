use graphql_parser::query as q;

use super::error::{QueryError, QueryExecutionError};

/// The result of running a query, if successful.
#[derive(Debug)]
pub struct QueryResult {
    pub data: Option<q::Value>,
    pub errors: Option<Vec<QueryError>>,
}

impl QueryResult {
    pub fn new(data: Option<q::Value>) -> Self {
        QueryResult { data, errors: None }
    }
}

impl From<QueryExecutionError> for QueryResult {
    fn from(e: QueryExecutionError) -> Self {
        let mut result = Self::new(None);
        result.errors = Some(vec![QueryError::from(e)]);
        result
    }
}

impl From<Vec<QueryExecutionError>> for QueryResult {
    fn from(e: Vec<QueryExecutionError>) -> Self {
        let mut result = Self::new(None);
        result.errors = Some(e.into_iter().map(|error| QueryError::from(error)).collect());
        result
    }
}
