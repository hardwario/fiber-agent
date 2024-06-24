#!/bin/bash
pip uninstall -y fiber
cd /app

mkdir -p /etc/fiber /var/fiber

cp /app/install/roles/system/files/rc.local /etc/rc.local
cp /app/install/roles/system/files/journald.conf /etc/systemd/journald.conf
cp /app/install/roles/fiber-packages/files/fiber-core.service /etc/systemd/system/fiber-core.service
cp /app/fiber/display/LiberationSans-BaDn.ttf /etc/fiber

pip install .

cp /app/config/config.yaml /data/config.yaml

systemctl enable fiber-core.service
systemctl restart fiber-core.service

rm -rf /app

mkdir -p /app