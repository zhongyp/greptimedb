// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use futures::{future, TryStreamExt};
use object_store::{Object, ObjectStore};
use regex::Regex;
use snafu::ResultExt;

use crate::error::{self, Result};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Filename(String),
    Dir,
}

pub struct Lister {
    object_store: ObjectStore,
    source: Source,
    path: String,
    regex: Option<Regex>,
}

impl Lister {
    pub fn new(
        object_store: ObjectStore,
        source: Source,
        path: String,
        regex: Option<Regex>,
    ) -> Self {
        Lister {
            object_store,
            source,
            path,
            regex,
        }
    }

    pub async fn list(&self) -> Result<Vec<Object>> {
        match &self.source {
            Source::Dir => {
                let streamer = self
                    .object_store
                    .object(&self.path)
                    .list()
                    .await
                    .context(error::ListObjectsSnafu { path: &self.path })?;

                streamer
                    .try_filter(|f| {
                        let res = self
                            .regex
                            .as_ref()
                            .map(|x| x.is_match(f.name()))
                            .unwrap_or(true);
                        future::ready(res)
                    })
                    .try_collect::<Vec<_>>()
                    .await
                    .context(error::ListObjectsSnafu { path: &self.path })
            }
            Source::Filename(filename) => {
                let obj = self
                    .object_store
                    .object(&format!("{}{}", self.path, filename));

                Ok(vec![obj])
            }
        }
    }
}
