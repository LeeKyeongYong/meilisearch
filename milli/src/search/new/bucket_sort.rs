use roaring::RoaringBitmap;

use super::logger::SearchLogger;
use super::ranking_rules::{BoxRankingRule, RankingRuleQueryTrait};
use super::SearchContext;
use crate::search::new::distinct::{apply_distinct_rule, distinct_single_docid, DistinctOutput};
use crate::Result;

pub fn bucket_sort<'ctx, Q: RankingRuleQueryTrait>(
    ctx: &mut SearchContext<'ctx>,
    mut ranking_rules: Vec<BoxRankingRule<'ctx, Q>>,
    query: &Q,
    universe: &RoaringBitmap,
    from: usize,
    length: usize,
    logger: &mut dyn SearchLogger<Q>,
) -> Result<Vec<u32>> {
    logger.initial_query(query);
    logger.ranking_rules(&ranking_rules);
    logger.initial_universe(universe);

    let distinct_fid = if let Some(field) = ctx.index.distinct_field(ctx.txn)? {
        ctx.index.fields_ids_map(ctx.txn)?.id(field)
    } else {
        None
    };

    if universe.len() < from as u64 {
        return Ok(vec![]);
    }
    if ranking_rules.is_empty() {
        if let Some(distinct_fid) = distinct_fid {
            let mut excluded = RoaringBitmap::new();
            let mut results = vec![];
            for docid in universe.iter() {
                if results.len() >= from + length {
                    break;
                }
                if excluded.contains(docid) {
                    continue;
                }
                distinct_single_docid(ctx.index, ctx.txn, distinct_fid, docid, &mut excluded)?;
                results.push(docid);
            }
            return Ok(results);
        } else {
            return Ok(universe.iter().skip(from).take(length).collect());
        };
    }

    let ranking_rules_len = ranking_rules.len();

    logger.start_iteration_ranking_rule(0, ranking_rules[0].as_ref(), query, universe);
    ranking_rules[0].start_iteration(ctx, logger, universe, query)?;

    let mut ranking_rule_universes: Vec<RoaringBitmap> =
        vec![RoaringBitmap::default(); ranking_rules_len];
    ranking_rule_universes[0] = universe.clone();

    let mut cur_ranking_rule_index = 0;

    /// Finish iterating over the current ranking rule, yielding
    /// control to the parent (or finishing the search if not possible).
    /// Update the candidates accordingly and inform the logger.
    macro_rules! back {
        () => {
            assert!(ranking_rule_universes[cur_ranking_rule_index].is_empty());
            logger.end_iteration_ranking_rule(
                cur_ranking_rule_index,
                ranking_rules[cur_ranking_rule_index].as_ref(),
                &ranking_rule_universes[cur_ranking_rule_index],
            );
            ranking_rule_universes[cur_ranking_rule_index].clear();
            ranking_rules[cur_ranking_rule_index].end_iteration(ctx, logger);
            if cur_ranking_rule_index == 0 {
                break;
            } else {
                cur_ranking_rule_index -= 1;
            }
        };
    }

    let mut results = vec![];
    let mut cur_offset = 0usize;

    /// Add the candidates to the results. Take `distinct`, `from`, `length`, and `cur_offset`
    /// into account and inform the logger.
    macro_rules! maybe_add_to_results {
        ($candidates:expr) => {
            // First apply the distinct rule on the candidates, reducing the universes if necessary
            let candidates = if let Some(distinct_fid) = distinct_fid {
                let DistinctOutput { remaining, excluded } = apply_distinct_rule(ctx, distinct_fid, $candidates)?;
                for universe in ranking_rule_universes.iter_mut() {
                    *universe -= &excluded;
                }
                remaining
            } else {
                $candidates.clone()
            };
            let len = candidates.len();
            // if the candidates are empty, there is nothing to do;
            if !candidates.is_empty() {
                // if we still haven't reached the first document to return
                if cur_offset < from {
                    // and if no document from this bucket can be returned
                    if cur_offset + (candidates.len() as usize) < from {
                        // then just skip the bucket
                        logger.skip_bucket_ranking_rule(
                            cur_ranking_rule_index,
                            ranking_rules[cur_ranking_rule_index].as_ref(),
                            &candidates,
                        );
                    } else {
                        // otherwise, skip some of the documents and add some of the rest, in order of ids
                        let all_candidates = candidates.iter().collect::<Vec<_>>();
                        let (skipped_candidates, candidates) =
                            all_candidates.split_at(from - cur_offset);
                        logger.skip_bucket_ranking_rule(
                            cur_ranking_rule_index,
                            ranking_rules[cur_ranking_rule_index].as_ref(),
                            &skipped_candidates.into_iter().collect(),
                        );
                        let candidates = candidates
                            .iter()
                            .take(length - results.len())
                            .copied()
                            .collect::<Vec<_>>();
                        logger.add_to_results(&candidates);
                        results.extend(&candidates);
                    }
                } else {
                    // if we have passed the offset already, add some of the documents (up to the limit)
                    let candidates =
                        candidates.iter().take(length - results.len()).collect::<Vec<u32>>();
                    logger.add_to_results(&candidates);
                    results.extend(&candidates);
                }
            }
            cur_offset += len as usize;
        };
    }

    while results.len() < length {
        // The universe for this bucket is zero or one element, so we don't need to sort
        // anything, just extend the results and go back to the parent ranking rule.
        if ranking_rule_universes[cur_ranking_rule_index].len() <= 1 {
            maybe_add_to_results!(&ranking_rule_universes[cur_ranking_rule_index]);
            ranking_rule_universes[cur_ranking_rule_index].clear();
            back!();
            continue;
        }

        let Some(next_bucket) = ranking_rules[cur_ranking_rule_index].next_bucket(ctx, logger, &ranking_rule_universes[cur_ranking_rule_index])? else {
            back!();
            continue;
        };

        logger.next_bucket_ranking_rule(
            cur_ranking_rule_index,
            ranking_rules[cur_ranking_rule_index].as_ref(),
            &ranking_rule_universes[cur_ranking_rule_index],
            &next_bucket.candidates,
        );

        debug_assert!(
            ranking_rule_universes[cur_ranking_rule_index].is_superset(&next_bucket.candidates)
        );
        ranking_rule_universes[cur_ranking_rule_index] -= &next_bucket.candidates;

        if cur_ranking_rule_index == ranking_rules_len - 1
            || next_bucket.candidates.len() <= 1
            || cur_offset + (next_bucket.candidates.len() as usize) < from
        {
            maybe_add_to_results!(&next_bucket.candidates);
            continue;
        }

        cur_ranking_rule_index += 1;
        ranking_rule_universes[cur_ranking_rule_index] = next_bucket.candidates.clone();
        logger.start_iteration_ranking_rule(
            cur_ranking_rule_index,
            ranking_rules[cur_ranking_rule_index].as_ref(),
            &next_bucket.query,
            &ranking_rule_universes[cur_ranking_rule_index],
        );
        ranking_rules[cur_ranking_rule_index].start_iteration(
            ctx,
            logger,
            &next_bucket.candidates,
            &next_bucket.query,
        )?;
    }

    Ok(results)
}