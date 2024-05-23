import subprocess
import yaml
from loguru import logger
import pydantic


def load_config_from_file(config_path, config_model):
        try:
            with open(config_path, 'r') as yaml_file:
                parsed_yaml = yaml.safe_load(yaml_file)
            return config_model(**parsed_yaml)
        except FileNotFoundError as e:
            logger.error(f'File not found: {e}')
            raise 
        except yaml.YAMLError as e:
            logger.error(f'YAML load error: {e}')
            raise

def save_config(config_path: str, config_model, payload: dict) -> None:
    try:
        config_model(**payload)
    except pydantic.ValidationError as e:
        logger.error(f'Validation error: {e}')
        raise

    try:
        with open(config_path, 'w') as yaml_file:
            yaml.safe_dump(payload, yaml_file, default_flow_style=False)
        logger.info('Configuration updated')
        restart_fiber_core_service()

    except yaml.YAMLError as e:
        logger.error(f'YAML save error: {e}')
        raise

def restart_fiber_core_service() -> None:
    try:
        subprocess.run(["systemctl", "restart", "fiber-core.service"], check=True)
        logger.info("fiber-core.service restarted successfully")
    except subprocess.CalledProcessError as e:
        logger.error(f"Failed to restart fiber-core.service: {e}")