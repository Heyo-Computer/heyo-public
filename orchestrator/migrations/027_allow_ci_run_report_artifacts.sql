DO $$
BEGIN
    IF to_regclass('public.orchestration_artifacts') IS NOT NULL THEN
        ALTER TABLE orchestration_artifacts
            DROP CONSTRAINT IF EXISTS orchestration_artifacts_kind_check;

        ALTER TABLE orchestration_artifacts
            ADD CONSTRAINT orchestration_artifacts_kind_check CHECK (
                kind IN (
                    'domain-map',
                    'integration-plan',
                    'patch-set',
                    'deploy-plan',
                    'deploy-plan-questions',
                    'deploy-plan-review',
                    'deploy-preflight',
                    'verification-report',
                    'deploy-spec',
                    'deploy-report',
                    'ci-run-report'
                )
            );
    END IF;
END $$;
