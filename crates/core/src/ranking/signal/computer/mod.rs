// Stract is an open source web search engine.
// Copyright (C) 2024 Stract ApS
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use crate::query::optic::AsSearchableRule;
use crate::query::Query;
use crate::schema::text_field::TextField;
use crate::Result;
use crate::{
    enum_map::EnumMap, fastfield_reader, schema::TextFieldEnum, webgraph::NodeID, webpage::Webpage,
};

use std::cell::RefCell;

use std::sync::Arc;

use tantivy::fieldnorm::FieldNormReader;
use tantivy::postings::SegmentPostings;
use tantivy::query::{Query as _, Scorer};
use tantivy::tokenizer::Tokenizer as _;

use tantivy::DocId;
use tantivy::DocSet;

use crate::webpage::region::RegionCount;

use crate::ranking::bm25::MultiBm25Weight;
use crate::ranking::models::linear::LinearRegression;
use crate::ranking::{inbound_similarity, query_centrality};

use super::{ComputedSignal, Signal, SignalCoefficient, SignalEnum, SignalScore};

mod order;
pub use order::SignalComputeOrder;

#[derive(Clone)]
pub struct TextFieldData {
    pub(super) postings: Vec<SegmentPostings>,
    pub(super) weight: MultiBm25Weight,
    pub(super) fieldnorm_reader: FieldNormReader,
}

pub struct RuleBoost {
    docset: Box<dyn Scorer>,
    boost: f64,
}

pub struct OpticBoosts {
    rules: Vec<RuleBoost>,
}

pub struct SegmentReader {
    text_fields: EnumMap<TextFieldEnum, TextFieldData>,
    optic_boosts: OpticBoosts,
    fastfield_reader: Arc<fastfield_reader::SegmentReader>,
}

impl SegmentReader {
    pub fn text_fields_mut(&mut self) -> &mut EnumMap<TextFieldEnum, TextFieldData> {
        &mut self.text_fields
    }

    pub fn fastfield_reader(&self) -> &fastfield_reader::SegmentReader {
        &self.fastfield_reader
    }
}

#[derive(Clone)]
pub struct QueryData {
    simple_terms: Vec<String>,
    optic_rules: Vec<optics::Rule>,
    selected_region: Option<crate::webpage::Region>,
}
impl QueryData {
    pub fn selected_region(&self) -> Option<crate::webpage::Region> {
        self.selected_region
    }
}

pub struct SignalComputer {
    query_data: Option<QueryData>,
    query_signal_coefficients: Option<SignalCoefficient>,
    segment_reader: Option<RefCell<SegmentReader>>,
    inbound_similarity: Option<RefCell<inbound_similarity::Scorer>>,
    fetch_time_ms_cache: Vec<f64>,
    update_time_cache: Vec<f64>,
    query_centrality: Option<RefCell<query_centrality::Scorer>>,
    region_count: Option<Arc<RegionCount>>,
    current_timestamp: Option<usize>,
    linear_regression: Option<Arc<LinearRegression>>,
    order: SignalComputeOrder,
}

impl Clone for SignalComputer {
    fn clone(&self) -> Self {
        let inbound_similarity = self
            .inbound_similarity
            .as_ref()
            .map(|scorer| RefCell::new(scorer.borrow().clone()));

        let query_centrality = self
            .query_centrality
            .as_ref()
            .map(|scorer| RefCell::new(scorer.borrow().clone()));

        Self {
            query_data: self.query_data.clone(),
            query_signal_coefficients: self.query_signal_coefficients.clone(),
            segment_reader: None,
            inbound_similarity,
            fetch_time_ms_cache: self.fetch_time_ms_cache.clone(),
            update_time_cache: self.update_time_cache.clone(),
            query_centrality,
            region_count: self.region_count.clone(),
            current_timestamp: self.current_timestamp,
            linear_regression: self.linear_regression.clone(),
            order: self.order.clone(),
        }
    }
}

impl std::fmt::Debug for SignalComputer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalComputer")
            .field(
                "query",
                &self
                    .query_data
                    .as_ref()
                    .map(|q| q.simple_terms.clone())
                    .unwrap_or_default(),
            )
            .finish()
    }
}

impl SignalComputer {
    pub fn new(query: Option<&Query>) -> Self {
        let query_signal_coefficients = query.as_ref().and_then(|q| q.signal_coefficients());

        let fetch_time_ms_cache: Vec<_> = (0..1000)
            .map(|fetch_time| 1.0 / (fetch_time as f64 + 1.0))
            .collect();

        let update_time_cache = (0..(3 * 365 * 24))
            .map(|hours_since_update| 1.0 / ((hours_since_update as f64 + 1.0).log2()))
            .collect();

        let query = query.as_ref().map(|q| QueryData {
            simple_terms: q.simple_terms().to_vec(),
            optic_rules: q
                .optics()
                .iter()
                .flat_map(|o| o.rules.iter())
                .filter(|rule| match rule.action {
                    optics::Action::Downrank(b) | optics::Action::Boost(b) => b != 0,
                    optics::Action::Discard => false,
                })
                .cloned()
                .collect(),
            selected_region: q.region().cloned(),
        });

        let mut s = Self {
            segment_reader: None,
            inbound_similarity: None,
            query_signal_coefficients,
            fetch_time_ms_cache,
            update_time_cache,
            query_centrality: None,
            region_count: None,
            current_timestamp: None,
            linear_regression: None,
            query_data: query,
            order: SignalComputeOrder::empty(),
        };

        s.order = SignalComputeOrder::new(&s);
        s.set_current_timestamp(chrono::Utc::now().timestamp() as usize);

        s
    }

    fn prepare_textfields(
        &self,
        tv_searcher: &tantivy::Searcher,
        segment_reader: &tantivy::SegmentReader,
    ) -> Result<EnumMap<TextFieldEnum, TextFieldData>> {
        let mut text_fields = EnumMap::new();
        let schema = tv_searcher.schema();

        if let Some(query) = &self.query_data {
            if !query.simple_terms.is_empty() {
                for signal in SignalEnum::all() {
                    if let Some(text_field) = signal.as_textfield() {
                        let tv_field = schema.get_field(text_field.name()).unwrap();
                        let simple_query = itertools::intersperse(
                            query.simple_terms.iter().map(|s| s.as_str()),
                            " ",
                        )
                        .collect::<String>();

                        let mut terms = Vec::new();
                        let mut tokenizer = text_field.indexing_tokenizer();
                        let mut stream = tokenizer.token_stream(&simple_query);

                        while let Some(token) = stream.next() {
                            let term = tantivy::Term::from_field_text(tv_field, &token.text);
                            terms.push(term);
                        }

                        if terms.is_empty() {
                            continue;
                        }

                        let fieldnorm_reader = segment_reader.get_fieldnorms_reader(tv_field)?;
                        let inverted_index = segment_reader.inverted_index(tv_field)?;

                        let mut matching_terms = Vec::with_capacity(terms.len());
                        let mut postings = Vec::with_capacity(terms.len());
                        for term in &terms {
                            if let Some(p) =
                                inverted_index.read_postings(term, text_field.record_option())?
                            {
                                postings.push(p);
                                matching_terms.push(term.clone());
                            }
                        }
                        let weight = MultiBm25Weight::for_terms(tv_searcher, &matching_terms)?;

                        text_fields.insert(
                            text_field,
                            TextFieldData {
                                postings,
                                weight,
                                fieldnorm_reader,
                            },
                        );
                    }
                }
            }
        }

        Ok(text_fields)
    }

    fn prepare_optic(
        &self,
        tv_searcher: &tantivy::Searcher,
        segment_reader: &tantivy::SegmentReader,
        fastfield_reader: &fastfield_reader::FastFieldReader,
    ) -> Vec<RuleBoost> {
        let mut optic_rule_boosts = Vec::new();

        if let Some(query) = &self.query_data {
            optic_rule_boosts = query
                .optic_rules
                .iter()
                .filter_map(|rule| rule.as_searchable_rule(tv_searcher.schema(), fastfield_reader))
                .map(|(_, rule)| RuleBoost {
                    docset: rule
                        .query
                        .weight(tantivy::query::EnableScoring::Enabled {
                            searcher: tv_searcher,
                            statistics_provider: tv_searcher,
                        })
                        .unwrap()
                        .scorer(segment_reader, 0.0)
                        .unwrap(),
                    boost: rule.boost,
                })
                .collect();
        }

        optic_rule_boosts
    }

    pub fn register_segment(
        &mut self,
        tv_searcher: &tantivy::Searcher,
        segment_reader: &tantivy::SegmentReader,
        fastfield_reader: &fastfield_reader::FastFieldReader,
    ) -> Result<()> {
        let fastfield_segment_reader = fastfield_reader.get_segment(&segment_reader.segment_id());
        let text_fields = self.prepare_textfields(tv_searcher, segment_reader)?;
        let optic_rule_boosts = self.prepare_optic(tv_searcher, segment_reader, fastfield_reader);

        self.segment_reader = Some(RefCell::new(SegmentReader {
            text_fields,
            fastfield_reader: fastfield_segment_reader,
            optic_boosts: OpticBoosts {
                rules: optic_rule_boosts,
            },
        }));

        Ok(())
    }

    pub fn set_query_centrality(&mut self, query_centrality: query_centrality::Scorer) {
        self.query_centrality = Some(RefCell::new(query_centrality));
    }

    pub fn set_inbound_similarity(&mut self, scorer: inbound_similarity::Scorer) {
        let mut scorer = scorer;
        scorer.set_default_if_precalculated(true);

        self.inbound_similarity = Some(RefCell::new(scorer));
    }

    pub fn set_region_count(&mut self, region_count: RegionCount) {
        self.region_count = Some(Arc::new(region_count));
    }

    pub fn set_current_timestamp(&mut self, current_timestamp: usize) {
        self.current_timestamp = Some(current_timestamp);
    }

    pub fn set_linear_model(&mut self, linear_model: Arc<LinearRegression>) {
        self.linear_regression = Some(linear_model);
    }

    pub fn query_centrality(&self, host_id: NodeID) -> Option<f64> {
        self.query_centrality
            .as_ref()
            .map(|scorer| scorer.borrow_mut().score(host_id))
    }

    pub fn inbound_similarity(&self, host_id: NodeID) -> f64 {
        self.inbound_similarity
            .as_ref()
            .map(|scorer| scorer.borrow_mut().score(&host_id))
            .unwrap_or_default()
    }

    /// Computes the scored signals for a given document.
    ///
    /// Important: This function assues that the docs a scored in ascending order of docid
    /// within their segment. If this invariant is not upheld, the documents will not have
    /// scores calculated for their text related signals. The wrong ranking will most likely
    /// be returned.
    /// This function also assumes that the segment reader has been set.
    pub fn compute_signals(&self, doc: DocId) -> impl Iterator<Item = Option<ComputedSignal>> + '_ {
        self.order.compute(doc, self)
    }

    pub fn boosts(&mut self, doc: DocId) -> Option<f64> {
        self.segment_reader.as_ref().map(|segment_reader| {
            let mut downrank = 0.0;
            let mut boost = 0.0;

            for rule in &mut segment_reader.borrow_mut().optic_boosts.rules {
                if rule.docset.doc() > doc {
                    continue;
                }

                if rule.docset.doc() == doc || rule.docset.seek(doc) == doc {
                    if rule.boost < 0.0 {
                        downrank += rule.boost.abs();
                    } else {
                        boost += rule.boost;
                    }
                }
            }

            if downrank > boost {
                let diff = downrank - boost;
                1.0 / (1.0 + diff)
            } else {
                boost - downrank + 1.0
            }
        })
    }

    pub fn precompute_score(&self, webpage: &Webpage) -> f64 {
        SignalEnum::all()
            .filter_map(|signal| {
                signal
                    .precompute(webpage, self)
                    .map(|value| ComputedSignal {
                        signal,
                        score: SignalScore {
                            coefficient: self.coefficient(&signal),
                            value,
                        },
                    })
            })
            .map(|computed| computed.score.coefficient * computed.score.value)
            .sum()
    }

    pub fn coefficient(&self, signal: &SignalEnum) -> f64 {
        self.query_signal_coefficients
            .as_ref()
            .map(|coefficients| coefficients.get(signal))
            .or_else(|| {
                self.linear_regression
                    .as_ref()
                    .and_then(|model| model.weights.get(*signal).copied())
            })
            .unwrap_or(signal.default_coefficient())
    }

    pub fn segment_reader(&self) -> Option<&RefCell<SegmentReader>> {
        self.segment_reader.as_ref()
    }

    pub fn fetch_time_ms_cache(&self) -> &[f64] {
        &self.fetch_time_ms_cache
    }

    pub fn current_timestamp(&self) -> Option<usize> {
        self.current_timestamp
    }

    pub fn update_time_cache(&self) -> &[f64] {
        &self.update_time_cache
    }

    pub fn region_count(&self) -> Option<&RegionCount> {
        self.region_count
            .as_ref()
            .map(|region_count| &**region_count)
    }

    pub fn query_data(&self) -> Option<&QueryData> {
        self.query_data.as_ref()
    }
}
