// Cuely is an open source web search engine.
// Copyright (C) 2022 Cuely ApS
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

use std::{collections::HashMap, sync::Arc};

use axum::{extract, response::IntoResponse, Extension, Json};

use super::State;

pub async fn route(
    extract::Query(params): extract::Query<HashMap<String, String>>,
    Extension(state): Extension<Arc<State>>,
) -> impl IntoResponse {
    if let Some(query) = params.get("q") {
        Json(state.autosuggest.suggestions(query).unwrap())
    } else {
        Json(Vec::new())
    }
}