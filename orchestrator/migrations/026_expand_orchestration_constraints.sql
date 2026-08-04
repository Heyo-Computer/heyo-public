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
                    'deploy-report'
                )
            );
    END IF;
END $$;

DO $$
BEGIN
    IF to_regclass('public.orchestration_approvals') IS NOT NULL THEN
        ALTER TABLE orchestration_approvals
            DROP CONSTRAINT IF EXISTS orchestration_approvals_kind_check;

        ALTER TABLE orchestration_approvals
            ADD CONSTRAINT orchestration_approvals_kind_check CHECK (
                kind IN ('plan', 'patch', 'deploy', 'deploy_questions')
            );
    END IF;
END $$;

DO $$
BEGIN
    IF to_regclass('public.orchestration_tool_call_logs') IS NOT NULL THEN
        ALTER TABLE orchestration_tool_call_logs
            DROP CONSTRAINT IF EXISTS orchestration_tool_call_logs_status_check;

        ALTER TABLE orchestration_tool_call_logs
            ADD CONSTRAINT orchestration_tool_call_logs_status_check CHECK (
                status IN ('started', 'completed', 'failed')
            );
    END IF;
END $$;
