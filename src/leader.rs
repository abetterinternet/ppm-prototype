//! Leader implementation
use crate::{
    aggregate::{
        dump_accumulators, prio3_verify_parameter, Accumulator, VerifyResponse, VerifyStartRequest,
        VerifyStartSubRequest,
    },
    collect::{
        CollectRequest, CollectResponse, EncryptedOutputShare, OutputShare, OutputShareRequest,
    },
    error::{handle_rejection, response_to_api_problem, IntoHttpApiProblem, ProblemDocumentType},
    hpke::{self, Role},
    parameters::{Parameters, TaskId},
    upload::{EncryptedInputShare, Report, ReportExtension},
    with_shared_value, Interval, Timestamp,
};
use ::hpke::Serializable;
use chrono::{DateTime, TimeZone, Utc};
use color_eyre::eyre::Result;
use http::StatusCode;
use http_api_problem::HttpApiProblem;
use prio::{
    field::FieldError,
    vdaf::{prio3::Prio3Sum64, suite::Suite, Aggregatable, Aggregator, Vdaf, VdafError},
};
use reqwest::Client;
use std::{
    cmp::Ordering,
    collections::HashMap,
    fmt::Debug,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use warp::{reply, Filter};

static LEADER_USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    "/",
    "leader"
);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("JSON parse error")]
    JsonParse(#[from] serde_json::Error),
    #[error("encryption error")]
    Encryption(#[from] crate::hpke::Error),
    #[error("unrecognized task ID")]
    UnrecognizedTask(TaskId),
    #[error("unknown HPKE config ID {0}")]
    UnknownHpkeConfig(u8),
    #[error("VDAF error")]
    Vdaf(#[from] VdafError),
    #[error("HTTP client error")]
    HttpClient(#[from] reqwest::Error),
    #[error("{1}")]
    HelperHttpRequest(StatusCode, String),
    #[error("bad protocol parameters")]
    Parameters(#[from] crate::parameters::Error),
    #[error("aggregate protocol error {0}")]
    AggregateProtocol(String),
    #[error("field error")]
    PrioField(#[from] FieldError),
    #[error("helper error")]
    HelperError(#[source] HttpApiProblem),
    #[error("invalid batch interval {0}")]
    InvalidBatchInterval(Interval),
    #[error("insufficient batch size {0}")]
    InsufficientBatchSize(u64),
    #[error("request exceeds the batch's privacy budget")]
    PrivacyBudgetExceeded,
    #[error("Length mismatch")]
    LengthMismatch,
}

impl IntoHttpApiProblem for Error {
    fn problem_document_type(&self) -> Option<ProblemDocumentType> {
        match self {
            Self::JsonParse(_) => Some(ProblemDocumentType::UnrecognizedMessage),
            // TODO: not all encryption errors will be client errors so we
            // perhaps need a bool field on Error::Encryption to indicate
            // client vs. server error
            Self::Encryption(_) => Some(ProblemDocumentType::UnrecognizedMessage),
            Self::UnrecognizedTask(_) => Some(ProblemDocumentType::UnrecognizedTask),
            Self::UnknownHpkeConfig(_) => Some(ProblemDocumentType::OutdatedConfig),
            Self::InvalidBatchInterval(_) => Some(ProblemDocumentType::InvalidBatchInterval),
            Self::InsufficientBatchSize(_) => Some(ProblemDocumentType::InsufficientBatchSize),
            Self::PrivacyBudgetExceeded => Some(ProblemDocumentType::PrivacyBudgetExceeded),
            Self::HelperError(_) => Some(ProblemDocumentType::HelperError),
            Self::HelperHttpRequest(_, _) => Some(ProblemDocumentType::HelperError),
            _ => None,
        }
    }

    fn source_problem_document(&self) -> Option<&HttpApiProblem> {
        if let Self::HelperError(problem_document) = self {
            Some(problem_document)
        } else {
            None
        }
    }
}

/// In-memory representation of an input stored by the leader
#[derive(Clone, Debug)]
pub struct StoredInputShare<A: Aggregator> {
    pub timestamp: Timestamp,
    pub leader_state: A::PrepareStep,
    pub leader_verifier_message: A::PrepareMessage,
    pub encrypted_helper_share: EncryptedInputShare,
    pub extensions: Vec<ReportExtension>,
}

impl<A: Aggregator> PartialEq for StoredInputShare<A> {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp.eq(&other.timestamp)
    }
}

impl<A: Aggregator> Eq for StoredInputShare<A> {}

impl<A: Aggregator> Ord for StoredInputShare<A> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.timestamp.cmp(&other.timestamp)
    }
}

impl<A: Aggregator> PartialOrd for StoredInputShare<A> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Implements endpoints the leader supports and tracks leader state.
#[derive(Clone, Debug)]
pub struct Leader {
    parameters: Parameters,
    hpke_config: hpke::Config,
    // TODO make Leader generic over Vdaf
    vdaf: Prio3Sum64,
    /// Inputs for which the leader has not yet received a VerifierMessage from
    /// the helper (though the leader may have already _sent_ a
    /// VerifierMessage). The vec is kept sorted so that the helper shares and
    /// verifier messages can be sent to helper in increasing order per RFCXXXX
    /// 4.3.1.
    unaggregated_inputs: Vec<StoredInputShare<Prio3Sum64>>,
    /// Accumulated sums over inputs that have been verified in conjunction with
    /// the helper. The key is the start of the batch window.
    accumulators: HashMap<DateTime<Utc>, Accumulator<<Prio3Sum64 as Vdaf>::AggregateShare>>,
    helper_state: Vec<u8>,
    http_client: Client,
}

impl Leader {
    pub fn new(parameters: &Parameters, hpke_config: &hpke::Config) -> Result<Self, Error> {
        Ok(Self {
            parameters: parameters.clone(),
            hpke_config: hpke_config.clone(),
            // TODO make leader generic over Vdaf
            vdaf: Prio3Sum64::new(Suite::Blake3, 2, 63)?,
            unaggregated_inputs: vec![],
            accumulators: HashMap::new(),
            helper_state: vec![],
            http_client: Client::builder().user_agent(LEADER_USER_AGENT).build()?,
        })
    }

    #[tracing::instrument(skip(self, report), err)]
    pub async fn handle_upload(&mut self, report: &Report) -> Result<(), Error> {
        debug!(?report, "obtained report");

        if report.task_id != self.parameters.task_id {
            return Err(Error::UnrecognizedTask(report.task_id));
        }

        let leader_share = &report.encrypted_input_shares[Role::Leader.index()];

        if leader_share.aggregator_config_id != self.hpke_config.id {
            return Err(Error::UnknownHpkeConfig(leader_share.aggregator_config_id));
        }

        // Decrypt and decode leader UploadMessage. We must create a new context
        // for each message or the nonces won't line up with the sender.
        let hpke_recipient = self.hpke_config.report_recipient(
            &report.task_id,
            Role::Leader,
            &leader_share.encapsulated_context,
        )?;

        let decrypted_input_share = hpke_recipient
            .decrypt_input_share(leader_share, &report.timestamp.associated_data())?;

        let input_share_message: <Prio3Sum64 as Vdaf>::InputShare =
            serde_json::from_slice(&decrypted_input_share)?;

        let state = self.vdaf.prepare_init(
            &prio3_verify_parameter(Role::Leader),
            &(),
            &report.timestamp.associated_data(),
            &input_share_message,
        )?;

        // Construct leader verifier message
        let (state, leader_verifier_message) = self.vdaf.prepare_start(state)?;

        self.unaggregated_inputs.push(StoredInputShare {
            timestamp: report.timestamp,
            leader_state: state,
            leader_verifier_message,
            encrypted_helper_share: report.encrypted_input_shares[Role::Helper.index()].clone(),
            extensions: report.extensions.clone(),
        });
        // TODO use an std::collections::BinaryHeap here for efficient
        // inserts
        self.unaggregated_inputs.sort_unstable();

        // Once we have 10 unaggregated inputs, send an aggregate request to
        // helper
        // TODO configure the threshold
        // TODO don't block upload requests on a synchronous aggregate txn
        if self.unaggregated_inputs.len() >= 10 {
            info!(
                sub_request_count = self.unaggregated_inputs.len(),
                "sending aggregate request to helper"
            );
            if let Err(e) = self.send_aggregate_request().await {
                error!(
                    "error when executing aggregate protocol with helper: {:?}",
                    e
                );
            }
        }

        Ok(())
    }

    #[tracing::instrument(err, skip(self))]
    async fn send_aggregate_request(&mut self) -> Result<(), Error> {
        let aggregate_sub_requests: Vec<VerifyStartSubRequest> = self
            .unaggregated_inputs
            .iter()
            .map(|stored_input| {
                Ok(VerifyStartSubRequest {
                    timestamp: stored_input.timestamp,
                    extensions: stored_input.extensions.clone(),
                    verify_message: serde_json::to_vec(&stored_input.leader_verifier_message)?,
                    helper_share: stored_input.encrypted_helper_share.clone(),
                })
            })
            .collect::<Result<_, serde_json::Error>>()?;

        let aggregate_request = VerifyStartRequest {
            task_id: self.parameters.task_id,
            aggregation_parameter: None,
            helper_state: self.helper_state.clone(),
            sub_requests: aggregate_sub_requests,
        };

        let http_response = self
            .http_client
            .post(self.parameters.aggregate_endpoint()?)
            .json(&aggregate_request)
            .send()
            .await?;
        let http_response_status = http_response.status();

        if !http_response_status.is_success() {
            return match response_to_api_problem(http_response).await {
                Ok(document) => Err(Error::HelperError(document)),
                Err(message) => Err(Error::HelperHttpRequest(http_response_status, message)),
            };
        }

        // At this point we got an HTTP 200 OK from helper, meaning it
        // successfully processed all the reports leader sent it (potentially
        // rejecting some due to bad proofs). That means we don't want to
        // re-send any of the reports we sent in a subsequent call to this
        // method, so reinitialize the leader's unaggregated inputs to empty.
        let leader_inputs = std::mem::take(&mut self.unaggregated_inputs);

        let aggregate_response: VerifyResponse = http_response.json().await?;

        if leader_inputs.len() != aggregate_response.sub_responses.len() {
            return Err(Error::AggregateProtocol(format!(
                "unexpected number of sub-responses in helper aggregate response. Got {} wanted {}",
                aggregate_response.sub_responses.len(),
                self.unaggregated_inputs.len()
            )));
        }

        for (leader_input, helper_response) in leader_inputs
            .into_iter()
            .zip(aggregate_response.sub_responses)
        {
            // Sub-responses from helper must appear in the same order as the
            // sub-requests sent by leader
            if leader_input.timestamp != helper_response.timestamp {
                return Err(Error::AggregateProtocol(format!(
                    "helper responses in wrong order. Wanted {}, got {}",
                    leader_input.timestamp, helper_response.timestamp,
                )));
            }

            // TODO: make this generic over Vdaf
            let helper_verifier_message: <Prio3Sum64 as Aggregator>::PrepareMessage =
                serde_json::from_slice(&helper_response.verification_message)?;

            let interval_start = leader_input
                .timestamp
                .time
                .interval_start(self.parameters.min_batch_duration);

            info!(
                timestamp = ?leader_input.timestamp,
                helper_verifier_message = ?helper_verifier_message,
                leader_verifier_message = ?leader_input,
                "verifying proof"
            );

            let output_share = match self.vdaf.prepare_finish(
                leader_input.leader_state,
                self.vdaf.prepare_preprocess([
                    helper_verifier_message,
                    leader_input.leader_verifier_message,
                ])?,
            ) {
                Ok(output_share) => output_share,
                Err(e) => {
                    let boxed_error: Box<dyn std::error::Error + 'static> = e.into();
                    warn!(
                        time = ?leader_input.timestamp,
                        error = boxed_error.as_ref(),
                        "proof did not check out for report"
                    );
                    continue;
                }
            };

            // Proof checked out -- sum the input share into the accumulator for
            // the batch interval corresponding to the report timestamp.
            if let Some(sum) = self.accumulators.get_mut(&interval_start) {
                sum.accumulated.accumulate(&output_share)?;
                sum.contributions += 1;
            } else {
                // This is the first input we have seen for this batch interval.
                // Initialize the accumulator.
                self.accumulators.insert(
                    interval_start,
                    Accumulator {
                        // TODO: we need the aggregation param for poplar1
                        accumulated: self.vdaf.aggregate(&(), [output_share])?,
                        contributions: 1,
                        privacy_budget: 0,
                    },
                );
            }
        }

        self.helper_state = aggregate_response.helper_state;

        dump_accumulators(&self.accumulators);

        Ok(())
    }

    #[tracing::instrument(skip(self, collect_request), err)]
    pub async fn handle_collect(
        &mut self,
        collect_request: &CollectRequest,
    ) -> Result<CollectResponse, Error> {
        if !self
            .parameters
            .validate_batch_interval(collect_request.batch_interval)
        {
            return Err(Error::InvalidBatchInterval(collect_request.batch_interval));
        }

        let num_intervals_in_request = collect_request
            .batch_interval
            .min_intervals_in_interval(self.parameters.min_batch_duration);

        let output_share_request = OutputShareRequest {
            task_id: collect_request.task_id,
            batch_interval: collect_request.batch_interval,
            helper_state: self.helper_state.clone(),
        };

        let http_response = self
            .http_client
            .post(self.parameters.output_share_endpoint()?)
            .json(&output_share_request)
            .send()
            .await?;
        let http_response_status = http_response.status();

        if !http_response_status.is_success() {
            return match response_to_api_problem(http_response).await {
                Ok(document) => Err(Error::HelperError(document)),
                Err(message) => Err(Error::HelperHttpRequest(http_response_status, message)),
            };
        }

        let helper_encrypted_output_share: EncryptedOutputShare = http_response.json().await?;

        let first_interval = collect_request
            .batch_interval
            .start
            .interval_start(self.parameters.min_batch_duration);

        let mut aggregate_shares = vec![];
        let mut total_contributions = 0;

        for i in 0..num_intervals_in_request {
            let interval_start = Utc.timestamp(
                first_interval.timestamp() + (i * self.parameters.min_batch_duration) as i64,
                0,
            );
            match self.accumulators.get_mut(&interval_start) {
                Some(accumulator) => {
                    if accumulator.privacy_budget == self.parameters.max_batch_lifetime {
                        return Err(Error::PrivacyBudgetExceeded);
                    }
                    aggregate_shares.push(accumulator.accumulated.clone());

                    accumulator.privacy_budget += 1;
                    total_contributions += accumulator.contributions;
                }
                None => {
                    // Most likely there are no contributions for this batch interval yet
                    warn!(
                        "no accumulator found for interval start {:?}",
                        interval_start
                    );
                    continue;
                }
            };
        }

        if total_contributions < self.parameters.min_batch_size {
            return Err(Error::InsufficientBatchSize(total_contributions));
        }

        // Merge aggregate shares into a single aggregate share
        let remaining_shares = aggregate_shares.split_off(1);
        for aggregate_share in remaining_shares.into_iter() {
            aggregate_shares[0].merge(&aggregate_share)?;
        }

        let output_share: OutputShare<Prio3Sum64> = OutputShare {
            sum: aggregate_shares.swap_remove(0),
            contributions: total_contributions,
        };

        let json_output_share = serde_json::to_vec(&output_share)?;

        let hpke_sender = self
            .parameters
            .collector_config
            .output_share_sender(&self.parameters.task_id, Role::Leader)?;

        let (payload, encapped) = hpke_sender
            .encrypt_output_share(output_share_request.batch_interval, &json_output_share)?;

        let leader_output_share = EncryptedOutputShare {
            collector_hpke_config_id: self.parameters.collector_config.id,
            encapsulated_context: encapped.to_bytes().to_vec(),
            payload,
        };

        Ok(CollectResponse {
            encrypted_output_shares: vec![leader_output_share, helper_encrypted_output_share],
        })
    }
}

pub async fn run_leader(ppm_parameters: Parameters, hpke_config: hpke::Config) -> Result<()> {
    let port = ppm_parameters.aggregator_urls[Role::Leader.index()]
        .port()
        .unwrap_or(80);
    let hpke_config_endpoint = hpke_config.warp_endpoint();

    let leader_aggregator = Arc::new(Mutex::new(Leader::new(&ppm_parameters, &hpke_config)?));

    let upload = warp::post()
        .and(warp::path("upload"))
        .and(warp::body::json())
        .and(with_shared_value(leader_aggregator.clone()))
        .and_then(|report: Report, leader: Arc<Mutex<Leader>>| async move {
            let mut leader = leader.lock().await;
            match leader.handle_upload(&report).await {
                Ok(()) => Ok(reply::with_status(reply(), StatusCode::OK)),
                Err(e) => Err(warp::reject::custom(
                    e.problem_document(&leader.parameters, "upload"),
                )),
            }
        })
        .with(warp::trace::named("upload"));

    let collect = warp::post()
        .and(warp::path("collect"))
        .and(warp::body::json())
        .and(with_shared_value(leader_aggregator.clone()))
        .and_then(
            |collect_request: CollectRequest, leader: Arc<Mutex<Leader>>| async move {
                let mut leader = leader.lock().await;
                match leader.handle_collect(&collect_request).await {
                    Ok(response) => Ok(reply::with_status(reply::json(&response), StatusCode::OK)),
                    Err(e) => Err(warp::reject::custom(
                        e.problem_document(&leader.parameters, "collect"),
                    )),
                }
            },
        )
        .with(warp::trace::named("collect"));

    let routes = hpke_config_endpoint
        .or(upload)
        .or(collect)
        .recover(handle_rejection)
        .with(warp::trace::request());

    info!("leader serving on 0.0.0.0:{}", port);
    warp::serve(routes)
        .run(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), port))
        .await;

    unreachable!()
}
