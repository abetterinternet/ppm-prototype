use crate::parameters::Parameters;
use http::StatusCode;
use http_api_problem::HttpApiProblem;
use std::{convert::Infallible, error::Error};
use warp::reject::Rejection;

/// Represents the possible URNs in PPM HTTP problem documents
pub(crate) enum ProblemDocumentType {
    UnrecognizedMessage,
    UnrecognizedTask,
    OutdatedConfig,
    InvalidProof,
    InvalidBatchInterval,
    InsufficientBatchSize,
    PrivacyBudgetExceeded,
    UnknownError,
}

impl From<ProblemDocumentType> for String {
    fn from(type_urn: ProblemDocumentType) -> Self {
        let problem_type = match type_urn {
            ProblemDocumentType::UnrecognizedMessage => "unrecognizedMessage",
            ProblemDocumentType::UnrecognizedTask => "unrecognizedTask",
            ProblemDocumentType::OutdatedConfig => "outdatedConfig",
            ProblemDocumentType::InvalidProof => "invalidProof",
            ProblemDocumentType::InvalidBatchInterval => "invalidBatchInterval",
            ProblemDocumentType::InsufficientBatchSize => "insufficientBatchSize",
            ProblemDocumentType::PrivacyBudgetExceeded => "privacyBudgetExceeded",
            ProblemDocumentType::UnknownError => "unknownError",
        };

        format!("url:ietf:params:ppm:error:{}", problem_type)
    }
}

/// Allows conversion into an `HttpApiProblem`. Intended for implementation by
/// the crate's various error types.
pub(crate) trait IntoHttpApiProblem: Error {
    /// Constructs an `HttpApiProblem` annotated with the PPM task ID and
    /// endpoint
    fn problem_document(
        &self,
        ppm_parameters: &Parameters,
        endpoint: &'static str,
    ) -> HttpApiProblem {
        match self.problem_document_type() {
            Some(problem_document_type) => {
                HttpApiProblem::new(StatusCode::BAD_REQUEST).type_url(problem_document_type)
            }
            None => HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
                .type_url(ProblemDocumentType::UnknownError),
        }
        .detail(self.to_string())
        .value("taskid", &ppm_parameters.task_id.to_string())
        .instance(endpoint)
    }

    /// Get problem document type and detail string for the error, or None for
    /// errors not captured by any of the PPM protocol's error types, in which
    /// case a problem document with HTTP status code 500 is constructed.
    fn problem_document_type(&self) -> Option<ProblemDocumentType>;
}

/// warp rejection handler that can be tacked on to routes to construct a
/// warp::Reply with appropriate status code and JSON body for an HTTP problem
/// document.
pub(crate) async fn handle_rejection(rejection: Rejection) -> Result<impl warp::Reply, Infallible> {
    // All our warp rejections should wrap a problem document, so crash if we
    // can't find one.
    let problem_document = rejection.find::<HttpApiProblem>().unwrap();

    Ok(warp::reply::with_status(
        warp::reply::json(problem_document),
        problem_document
            .status
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
    ))
}