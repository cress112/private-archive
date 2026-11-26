# Archive infrastructure

This configuration defines a **new** `cress-archived-data` bucket in `us-east-1`, the `private-archive-role` role, and the `private-archive-source` IAM user. No AWS resources were provisioned during implementation.

On 2026-11-23 the operator requested configuration and static validation only: provisioning is deferred until the remaining implementation work is finished. No live plan/apply is part of the current work. Use the [acceptance runbook](../../../docs/AWS_ACCEPTANCE_RUNBOOK.md) after the operator provisions the infrastructure; record live acceptance separately from Docker/static validation.

The bucket blocks public access, uses SSE-S3 (`AES256`), and remains in the default never-versioned state (no PutBucketVersioning request). Its only Lifecycle rule aborts incomplete multipart uploads beginning one day after initiation. The rule does not guarantee deletion at an exact hour. There are no object expiration or archive transition rules. A separate versioning resource is intentionally absent: `Suspended` is a different state from a newly created unversioned bucket. A nonempty bucket is not force-deleted.

The role trusts only the dedicated source user. The user's sole policy allows `sts:AssumeRole` on that role. The role allows `ListBucket` on this bucket and `GetObject`, `PutObject`, `DeleteObject`, `RestoreObject`, and `AbortMultipartUpload` on its objects. Restore is included for the agreed Phase 2 restore operation. It does not allow bucket discovery, bucket administration, version operations, or enumerating/aborting arbitrary multipart uploads. The application uses its own known upload ID when aborting.

The trust policy and both inline policies use the AWS provider's [`aws_iam_policy_document` data source](https://github.com/hashicorp/terraform-provider-aws/blob/v6.73.0/website/docs/d/iam_policy_document.html.markdown). Statements, actions, resources, and principals are expressed through its schema; each IAM resource consumes the generated `.json`. The [role documentation](https://github.com/hashicorp/terraform-provider-aws/blob/v6.73.0/website/docs/r/iam_role.html.markdown#example-of-using-data-source-for-assume-role-policy) also documents this pattern for `assume_role_policy`.

Access keys and console login profiles are deliberately absent from Terraform. No secret values are written to its configuration or outputs. The provider uses the operator's standard external AWS credentials; the application's dedicated role is not the identity used to provision resources.

Validation performed with Terraform 1.17.1 and AWS provider 6.73.0 (locked in `.terraform.lock.hcl`):

```bash
terraform init -backend=false -input=false
terraform fmt -check
terraform validate
```

These checks do not establish that the bucket name is available, that the operator has provisioning permissions, or that AWS IAM, AssumeRole, Deep Archive, and Lifecycle behavior works in a real account. No `plan` or `apply` was run. Deployment is a separate operator action: inspect a real plan for the intended account before applying it. Local state may contain account/resource identifiers; keep it outside version control.

After provisioning, follow [the CLI setup guide](../../../docs/CLI_SETUP.md) to create the source user's key in the AWS console and configure the two external profiles. The launch wrappers use AWS CLI v2 to resolve the role profile and pass the resolved credentials and region to the application through environment variables. The application uses the SDK default credential chain without a named profile selection. `role_arn`, `source_user_name`, `bucket_name`, and `region` are the only outputs.

Primary references: [S3 policy action mapping](https://docs.aws.amazon.com/AmazonS3/latest/userguide/using-with-s3-policy-actions.html), [Abort incomplete uploads](https://docs.aws.amazon.com/AmazonS3/latest/userguide/mpu-abort-incomplete-mpu-lifecycle-config.html), [AssumeRole profile configuration](https://docs.aws.amazon.com/sdkref/latest/guide/feature-assume-role-credentials.html).

## Operational handoff (2026-11-22, D-11/D-12)

This is the infrastructure configuration used for normal operation as well as final acceptance. Keep the bucket, IAM role, source user, and Terraform state after acceptance; remove only test objects, test index objects, and multipart uploads started by the test, and verify the bucket is empty. Do not destroy and recreate the infrastructure as test teardown. The application role continues to use the same least-privilege policy.

The intended bucket name is `cress-archived-data`. If it is unavailable, stop and consult the user; do not select a fallback name or import an existing bucket automatically. Live provisioning and acceptance take place only in the final phase under the [final AWS validation plan](../../../docs/FINAL_AWS_VALIDATION_PLAN.md).

The operator later authorized application-level live AWS acceptance using the existing resources. Infrastructure inspection and plan/apply remain excluded by explicit instruction; only local IaC may be reviewed or corrected if application behavior fails. See [live acceptance status](../../../docs/AWS_VALIDATION.md). No infrastructure APIs were used by the acceptance driver.
