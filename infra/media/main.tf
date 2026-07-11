# Hetzner Cloud media server for VoxTranslate webinars (F1-0).
# Provisions a small shared-vCPU box + a firewall (SSH, HTTP/S, WebRTC UDP). The
# box then runs MediaMTX + Caddy via docker compose — see /DEPLOY-HETZNER.md.
#
#   terraform init
#   terraform apply -var="admin_ip=$(curl -s ifconfig.me)/32" -var="hcloud_token=..."

terraform {
  required_providers {
    hcloud = {
      source  = "hetznercloud/hcloud"
      version = "~> 1.49"
    }
  }
}

variable "hcloud_token" {
  description = "Hetzner Cloud API token (Read/Write), from Security → API Tokens."
  type        = string
  sensitive   = true
}

variable "server_type" {
  description = "cx32 (Intel/AMD shared, ~EUR 6.80/mo) or cax21 (ARM Ampere, best price/perf, ~EUR 8/mo). Do NOT use CCX/CPX dedicated — 2-3x the price and unneeded here."
  type        = string
  default     = "cx32"
}

variable "location" {
  description = "nbg1 / fsn1 (Germany) or hel1 (Finland) — central for EU hosts. The CDN handles guest delivery; this only affects the presenter's ingest latency."
  type        = string
  default     = "nbg1"
}

variable "ssh_public_key_path" {
  description = "SSH public key installed on the box (ssh-keygen -t ed25519 -f ~/.ssh/vox_media)."
  type        = string
  default     = "~/.ssh/vox_media.pub"
}

variable "admin_ip" {
  description = "Your public IP in CIDR form (e.g. 203.0.113.7/32) — the only source allowed to SSH."
  type        = string
}

provider "hcloud" {
  token = var.hcloud_token
}

resource "hcloud_ssh_key" "media" {
  name       = "vox-media"
  public_key = file(var.ssh_public_key_path)
}

resource "hcloud_firewall" "media" {
  name = "vox-media-fw"

  # SSH — your IP only.
  rule {
    direction  = "in"
    protocol   = "tcp"
    port       = "22"
    source_ips = [var.admin_ip]
  }
  # HTTP — Let's Encrypt HTTP-01 challenge (Caddy).
  rule {
    direction  = "in"
    protocol   = "tcp"
    port       = "80"
    source_ips = ["0.0.0.0/0", "::/0"]
  }
  # HTTPS — WHIP signaling + HLS delivery.
  rule {
    direction  = "in"
    protocol   = "tcp"
    port       = "443"
    source_ips = ["0.0.0.0/0", "::/0"]
  }
  # WebRTC ICE media (UDP) — ESSENTIAL; without it the WHIP ingest never connects.
  rule {
    direction  = "in"
    protocol   = "udp"
    port       = "8189"
    source_ips = ["0.0.0.0/0", "::/0"]
  }
}

resource "hcloud_server" "media" {
  name         = "vox-media-01"
  image        = "ubuntu-24.04"
  server_type  = var.server_type
  location     = var.location
  ssh_keys     = [hcloud_ssh_key.media.id]
  firewall_ids = [hcloud_firewall.media.id]

  public_net {
    ipv4_enabled = true
    ipv6_enabled = true
  }
}

output "ipv4" {
  value       = hcloud_server.media.ipv4_address
  description = "Point ingest.voxtranslate.app at this IP as a DNS-only (grey-cloud) A record."
}
