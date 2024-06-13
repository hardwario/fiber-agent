import subprocess
import yaml
from loguru import logger
from pydantic import ValidationError


def load_config_from_file(config_path: str, config_model):
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
        except ValidationError as e:
            raise

def save_config(config_path: str, config_model, payload: dict) -> None:
    try:
        config_model(**payload)
    except ValidationError as e:
        logger.error(f'Validation error: {e.json()}')
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
        logger.info("Fiber service restarted successfully")
    except subprocess.CalledProcessError as e:
        logger.error(f"Failed to restart fiber-core.service: {e}")