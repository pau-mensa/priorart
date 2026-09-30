# Retrieval recipes

A recipe decides how collections are indexed and how a search across them is
ranked. priorart ships one, `gather::Bm25Recipe`. A build with its own recipe is
a separate crate that depends on an exact priorart version and supplies the
recipe to the same command line:

```rust
use std::process::ExitCode;
use std::sync::Arc;

fn main() -> ExitCode {
    priorart::cli::run(|settings| Ok(Arc::new(MyRecipe::open(&settings.data_dir)?)))
}
```

The closure runs only for `priorart serve`; `admin` and `mcp` never build a
recipe. `priorart::lateweave` re-exports the lateweave version priorart uses.

## The contract

`recipe::Recipe` and `recipe::CollectionIndex`:

- `load` builds a collection's index from the latest revision of each live
  record. It runs on the first search and again after an eviction, a restart,
  or a failed update.
- `upsert` and `remove` run after every committed write, under the
  collection's lock. An error discards the index and the next search rebuilds
  it; the write still succeeds and is counted in
  `priorart_index_update_failures_total`.
- `query` turns the text into the lateweave query, adding any query features.
- `gather_limit` sets how many candidates to gather for a requested limit.
- `pipeline` builds one lateweave pipeline over every selected collection,
  given in collection ID order. Documents are keyed by (collection ID, record
  ID) and must be indexed records; scores must compare across collections and
  ties must break deterministically. The pipeline receives the metadata filter
  as a subset.

The service keeps everything else: authorization, locking, caching, filters,
the revision each hit names, excerpts, the search log, and metrics. A recipe
that keeps state on disk puts it in its own file in the data directory; the
main database and its migrations are not a recipe's to change.

These traits may change in any minor release before 1.0.
