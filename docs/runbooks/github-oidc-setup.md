# Runbook — GitHub Actions → AWS via OIDC (no long-lived keys)

> **Goal:** let the deploy workflows assume an AWS IAM role using GitHub's
> short-lived OIDC token, so the repo holds **no** long-lived AWS access keys.
> **Referenced by:** `docs/runbooks/aws-deploy.md` step 1.
> **Consumed by:** `.github/workflows/deploy-aws.yml`, `aws-autopilot.yml`,
> `aws-control.yml` — each declares `permissions: id-token: write` and calls
> `aws-actions/configure-aws-credentials@v4` with
> `role-to-assume: ${{ secrets.AWS_ROLE_ARN }}` and
> `aws-region: ${{ vars.AWS_REGION || 'ap-south-1' }}`.

---

## What you end up with

| Thing | Value |
|---|---|
| IAM OIDC identity provider | `token.actions.githubusercontent.com` |
| IAM role | `tv-github-deploy-role` |
| Trust | only this repo's workflows (`repo:SJParthi/tickvault:*`) |
| Repo secret | `AWS_ROLE_ARN` = the role's ARN |
| Repo variable | `AWS_REGION` = `ap-south-1` (optional; workflow defaults to it) |
| Long-lived keys | **none** |

---

## Step 1 — Create the GitHub OIDC identity provider (once per AWS account)

```bash
aws iam create-open-id-connect-provider \
  --url https://token.actions.githubusercontent.com \
  --client-id-list sts.amazonaws.com \
  --thumbprint-list 6938fd4d98bab03faadb97b34396831e3780aea1
```

If it already exists you'll get `EntityAlreadyExists` — that's fine, reuse it.
(AWS now validates the thumbprint internally; the value above is the documented
GitHub root thumbprint.)

## Step 2 — Create the role with a repo-scoped trust policy

`trust-policy.json` (replace `<ACCOUNT_ID>`):

```json
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Principal": { "Federated": "arn:aws:iam::<ACCOUNT_ID>:oidc-provider/token.actions.githubusercontent.com" },
    "Action": "sts:AssumeRoleWithWebIdentity",
    "Condition": {
      "StringEquals": { "token.actions.githubusercontent.com:aud": "sts.amazonaws.com" },
      "StringLike":   { "token.actions.githubusercontent.com:sub": "repo:SJParthi/tickvault:*" }
    }
  }]
}
```

> **Tighten when possible:** narrow the `sub` from `:*` to a specific ref
> (e.g. `repo:SJParthi/tickvault:ref:refs/heads/main`) or environment once the
> deploy flow is settled. `:*` trusts any branch/PR workflow in the repo.

```bash
aws iam create-role \
  --role-name tv-github-deploy-role \
  --assume-role-policy-document file://trust-policy.json
```

## Step 3 — Attach least-privilege deploy permissions

The deploy workflows start/stop the EC2 instance and run SSM. Grant only what
they use (scope `Resource` to the specific instance / SSM paths in prod):

```bash
aws iam put-role-policy \
  --role-name tv-github-deploy-role \
  --policy-name tv-deploy \
  --policy-document file://deploy-permissions.json
```

`deploy-permissions.json` should cover (minimally):
`ec2:StartInstances`, `ec2:StopInstances`, `ec2:DescribeInstances`,
`ec2:DescribeInstanceStatus`, `ssm:SendCommand`, `ssm:GetCommandInvocation`,
plus `ssm:GetParameter*` on `/tickvault/<env>/*`. Do **not** grant `*:*`.

## Step 4 — Wire the role into the repo

- **Repo → Settings → Secrets and variables → Actions → Secrets:**
  add `AWS_ROLE_ARN` = `arn:aws:iam::<ACCOUNT_ID>:role/tv-github-deploy-role`.
- **Variables (optional):** add `AWS_REGION` = `ap-south-1`
  (the workflows already default to `ap-south-1` if unset).

## Step 5 — Verify

Re-run a deploy workflow (or its dry-run). In the
`aws-actions/configure-aws-credentials` step you should see the role assumed
with a short-lived session and no static-key warning. Confirm with a harmless
call in the workflow, e.g. `aws sts get-caller-identity`.

---

## Done = remove the temporary workaround

`aws-deploy.md` step 1 currently says "for now, use long-lived credentials as a
temporary workaround." Once Steps 1–5 pass, **delete any long-lived
`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` repo secrets** — OIDC fully
replaces them.

---

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `Not authorized to perform sts:AssumeRoleWithWebIdentity` | Trust-policy `sub`/`aud` mismatch — confirm `repo:SJParthi/tickvault:*` and `aud = sts.amazonaws.com`. |
| `Credentials could not be loaded` in the workflow | Missing `permissions: id-token: write` in the job, or `AWS_ROLE_ARN` secret unset. |
| `No OpenIDConnect provider found` | Step 1 not done in this account, or wrong account id in the provider ARN. |

---

## Cross-references

- `docs/runbooks/aws-deploy.md` — the deploy flow that consumes this role.
- `.github/workflows/deploy-aws.yml` — the workflow using `AWS_ROLE_ARN`.
