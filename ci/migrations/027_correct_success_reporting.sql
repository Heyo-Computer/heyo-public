-- A retry warning is historical event data, not the final error of a job that
-- later passed. Keep it in ci_event_outbox and stop presenting it as failure.
UPDATE ci_job
SET error = NULL
WHERE status IN ('success', 'skipped')
  AND error IS NOT NULL;

-- Make already-completed controller rollouts identify what was deployed. New
-- rollouts include the public origin as well; old rows did not persist it.
UPDATE ci_service_deployment
SET message = format(
    'Deployed CI controller `%s` from revision %s; verified the exact executable through public health; submissions reopened.',
    service_id,
    sha
)
WHERE id LIKE 'ci-controller-%'
  AND status = 'passed'
  AND message = 'Exact replacement binary verified through public health; submissions reopened.';
