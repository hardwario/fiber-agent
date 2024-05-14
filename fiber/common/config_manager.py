import yaml
from loguru import logger
import pydantic


class ConfigManagerError(Exception):
    pass

class ConfigManager:
    def __init__(self, config_path: str, config_model) -> None:
        self.config_path = config_path
        self.config_model = config_model
        self.config_data = self.load_config_from_file()

    def load_config_from_file(self):
        try:
            with open(self.config_path, 'r') as yaml_file:
                parsed_yaml = yaml.safe_load(yaml_file)
                logger.info(f'Configuration loaded: {parsed_yaml} with type {type(parsed_yaml)}')
            return self.config_model(**parsed_yaml)
        except FileNotFoundError as e:
            logger.error(f'File not found: {e}')
            raise ConfigManagerError(f'Configuration file not found: {e}')
        except yaml.YAMLError as e:
            logger.error(f'YAML load error: {e}')
            raise ConfigManagerError(f'Configuration load error: {e}')

    def save_config(self) -> None:
        try:
            with open(self.config_path, 'w') as yaml_file:
                yaml.safe_dump(self.config_data.dict(), yaml_file, default_flow_style=False)
            logger.info('Configuration updated')
        except yaml.YAMLError as e:
            logger.error(f'YAML save error: {e}')
            raise ConfigManagerError(f'Configuration save error: {e}')
