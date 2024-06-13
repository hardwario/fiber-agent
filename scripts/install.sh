#!/bin/bash
pip uninstall -y fiber

cd /app

cp /app/install/roles/system/files/rc.local /etc/rc.local

cp /app/install/roles/system/files/journald.conf /etc/systemd/journald.conf

cp /app/install/roles/fiber-packages/files/fiber-core.service /etc/systemd/system/fiber-core.service

python /app/fiber/setup.py install

mkdir /etc/fiber -p

mkdir /var/fiber -p

cp /app/config/config.yaml /etc/fiber/config.yaml

systemctl enable fiber-core.service

systemctl start fiber-core.service

systemctl restart fiber-core.service

# tmpfs /var/log  tmpfs defaults,noatime,nosuid,mode=0755,size=512m 0 0

# echo "Running Ansible playbook to install..."

# ansible-playbook -i /app/install/hosts /app/install/install.yml --ask-pass -vvv

# echo "Installation complete."

