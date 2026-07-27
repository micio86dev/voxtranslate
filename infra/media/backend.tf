###############################################################################
# Remote state on Cloudflare R2 (S3-compatible).
#
# The state file records the identity of every live box. Kept only on one laptop
# it is one disk failure away from the reconstruction this project already had to
# do once — see README.md § "Reconstructing the state".
#
# R2 rather than S3 because this account already runs R2 (vox-web-assets,
# vox-voices) and R2 has no egress charge. Bucket: vox-terraform-state.
#
# PARTIAL CONFIGURATION on purpose. The bucket, endpoint and credentials live in
# `backend.hcl`, which is gitignored: this repository is PUBLIC, and the endpoint
# embeds the Cloudflare account ID. Initialise with:
#
#   terraform init -backend-config=backend.hcl
#
# (./tf-import.sh passes that automatically.)
###############################################################################
terraform {
  backend "s3" {
    key = "media/terraform.tfstate"
    # R2 has one region and calls it `auto`.
    region = "auto"

    # State locking WITHOUT DynamoDB: Terraform writes a `.tflock` object next to
    # the state using an If-None-Match conditional PUT, which R2 supports on
    # PutObject. Generally available since Terraform 1.11 (this repo is on 1.15).
    # Without it two concurrent applies could silently clobber each other.
    use_lockfile = true

    # R2 is S3-compatible, not S3. Each of these disables an AWS-only assumption
    # that would otherwise fail before the first request is even sent.
    skip_credentials_validation = true # no AWS STS to validate against
    skip_region_validation      = true # `auto` is not an AWS region name
    skip_requesting_account_id  = true # no AWS account to look up
    skip_metadata_api_check     = true # no EC2 instance metadata endpoint
    skip_s3_checksum            = true # R2 rejects AWS's default trailing checksums
    use_path_style              = true # R2 addresses buckets by path, not subdomain
  }
}
