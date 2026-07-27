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
  description = "cx33 (Intel/AMD shared, 4 vCPU / 8 GB, ~EUR 5.5-6.6/mo) or cax21 (ARM Ampere, if available in your location). Do NOT use CCX/CPX dedicated — 2-3x the price and unneeded here."
  type        = string
  default     = "cx33"
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

###############################################################################
# READ REPLICAS
#
# A replica pulls each webinar from the origin over RTSP (on demand) and serves
# LL-HLS to guests, so guest bandwidth no longer competes with the host's WHIP
# ingest. The origin's outbound load becomes ~one stream copy PER REPLICA
# instead of one per viewer.
#
# Why replicas and not a CDN: Low-Latency HLS playlists CANNOT be cached. The
# blocking reload is a long-poll, which no major CDN can cache or coalesce, so
# every playlist request reaches the origin regardless of cache settings. The
# documented CDN path requires turning the low-latency variant OFF — which puts
# playback back at ~6 s and breaks the deliberate sync with translated audio.
# See https://mediamtx.org/docs/features/scaling
#
# Separately: serving third-party live video through Cloudflare's CDN is not
# permitted on Free/Pro/Business plans, so the playback hostname stays grey-cloud.
###############################################################################

variable "replica_count" {
  description = "Read replicas serving guest playback. 0 = origin serves guests directly (the pre-replica topology). Each replica adds roughly one cx33 of guest capacity."
  type        = number
  default     = 1
}

# RTSP between origin and replicas rides a private network: no public exposure
# (8554 is never opened in either firewall), no egress billing, and no TLS to
# manage on an internal hop.
resource "hcloud_network" "media" {
  count    = var.replica_count > 0 ? 1 : 0
  name     = "vox-media-net"
  ip_range = "10.10.0.0/16"
}

resource "hcloud_network_subnet" "media" {
  count        = var.replica_count > 0 ? 1 : 0
  network_id   = hcloud_network.media[0].id
  type         = "cloud"
  network_zone = "eu-central"
  ip_range     = "10.10.1.0/24"
}

# Pinned private IPs so the replica config can hardcode the origin's RTSP address.
resource "hcloud_server_network" "origin" {
  count     = var.replica_count > 0 ? 1 : 0
  server_id = hcloud_server.media.id
  subnet_id = hcloud_network_subnet.media[0].id
  ip        = "10.10.1.10"
}

resource "hcloud_server_network" "replica" {
  count     = var.replica_count
  server_id = hcloud_server.replica[count.index].id
  subnet_id = hcloud_network_subnet.media[0].id
  ip        = "10.10.1.${20 + count.index}"
}

# No 8189/udp here: a replica serves HLS only and never terminates WebRTC.
resource "hcloud_firewall" "replica" {
  count = var.replica_count > 0 ? 1 : 0
  name  = "vox-media-replica-fw"

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
  # HTTPS — LL-HLS delivery to guests. This is the only traffic a replica serves.
  rule {
    direction  = "in"
    protocol   = "tcp"
    port       = "443"
    source_ips = ["0.0.0.0/0", "::/0"]
  }
}

resource "hcloud_server" "replica" {
  count        = var.replica_count
  name         = "vox-media-replica-${format("%02d", count.index + 1)}"
  image        = "ubuntu-24.04"
  server_type  = var.server_type
  location     = var.location
  ssh_keys     = [hcloud_ssh_key.media.id]
  firewall_ids = [hcloud_firewall.replica[0].id]

  public_net {
    ipv4_enabled = true
    ipv6_enabled = true
  }
}

output "replica_ipv4" {
  value       = hcloud_server.replica[*].ipv4_address
  description = "Point hls.voxtranslate.app at these IPs as DNS-only (grey-cloud) A records, then set MEDIA_HLS_HOST=hls.voxtranslate.app on the control plane. Grey, not orange: Cloudflare does not permit third-party live video through the CDN on Free/Pro/Business."
}

output "origin_private_ip" {
  value       = var.replica_count > 0 ? "10.10.1.10" : null
  description = "The origin's private address — what each replica's mediamtx.yml pulls RTSP from."
}
