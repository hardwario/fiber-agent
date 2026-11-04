# Public demo

The portal with the three-Hub mock cluster (`dev/mock/README.md`) on one
DigitalOcean Droplet, for customers to click through. Everything in it is
synthetic and reproducible: the Droplet can be deleted and rebuilt in minutes.

```
caddy ──► portal ◄── mosquitto ◄── mock-server / mock-cold-store / mock-truck
 80/443    8088       1883 (compose network only)
```

## Release

The tag is the whole release. There is no step after it.

```bash
git tag demo-$(date +%F) && git push origin --tags
```

The `portal:demo-image` job builds `portal-demo` and `portal-demo-mock` from
this directory's Dockerfiles and pushes them to the project registry as
`:<tag>` and `:latest`. Any commit can be tagged, a `dev-*` branch included.

The Droplet then updates itself: the `watchtower` container in
`docker-compose.yml` polls the registry every 60 s and recreates the portal
and the three mock Hubs when `:latest` moves. Tag to page is the image build
plus up to a minute. Nobody logs in to deploy, and nothing has to remember to.

Watching it happen, if you want to:

```bash
ssh proximos-demo 'docker logs --tail 20 proximos-demo-watchtower-1'
```

A deploy of the compose file itself (this directory) is still a copy up, since
the file is what tells the box what to run:

```bash
scp -r portal/packaging/demo/. proximos-demo:/opt/proximos-demo/
ssh proximos-demo 'cd /opt/proximos-demo && docker compose up -d'
```

## Droplet

`s-1vcpu-1gb` in `fra1`, Ubuntu 24.04, cloud firewall 22 + 80 + 443. The whole
stack idles under 500 MB; the swap file below is the safety margin for a
1 GB box.

```bash
# on the Droplet, once
fallocate -l 1G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile \
  && echo '/swapfile none swap sw 0 0' >> /etc/fstab
apt-get install -y docker.io docker-compose-v2
docker login registry.hardwario.com     # a deploy token with read_registry

# from the repo
scp -r portal/packaging/demo root@<ip>:/opt/proximos-demo
```

Then in `/opt/proximos-demo`:

```bash
printf 'DEMO_SITE=:80\n' > .env         # later: DEMO_SITE=demo.example.com
docker compose up -d                    # watchtower keeps it current from here
```

## One-time setup and the golden snapshot

Open the portal, create the admin on `/setup`, pair the Hubs on the `/hubs`
page (the server pairs without a code; `cold-store` is `DEF456`, `truck` is
`GHI789`), create the shared customer account under `/settings/users`. Then:

```bash
./golden.sh save
(crontab -l 2>/dev/null; echo '0 3 * * * /opt/proximos-demo/golden.sh restore') | crontab -
```

The 03:00 restore is what keeps the demo usable: whatever customers acked,
renamed, deleted or re-passworded during the day is gone by morning. Redo
`golden.sh save` after any deliberate change to accounts or pairing.

## TLS

Once DNS points at the Droplet: `DEMO_SITE=<hostname>` in `.env`,
`cookie_secure: true` in `config.yaml`, `docker compose up -d`. Caddy obtains
and renews the certificate.

## Update

Tag and push — see Release. The box pulls the new image itself.

Migrations run on portal start; the golden snapshot from an older image keeps
working because migrations are append-only.

After a deliberate change to accounts or pairing, `./golden.sh save` — the
03:00 restore would otherwise put the old snapshot back over it, and that
applies to a new image the same as to a customer's clicking.
