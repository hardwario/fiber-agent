#!/bin/bash

echo "Running Ansible playbook to install..."

# Путь до вашего файла hosts и install.yml может отличаться
ansible-playbook -i /home/fiber/install/hosts /home/fiber/install/install.yml --ask-pass -K

echo "Installation complete."