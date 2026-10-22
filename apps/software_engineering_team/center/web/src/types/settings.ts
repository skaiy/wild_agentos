export interface ServerConfig {
  apiBaseUrl: string;
  wsBaseUrl: string;
  temporalHost: string;
}

export interface LLMConfig {
  apiKey: string;
  baseUrl: string;
  model: string;
  temperature: number;
  maxTokens: number;
}

export interface AgentOSConfig {
  grpcAddress: string;
  grpcTimeout: number;
}

export interface RuntimeConfig {
  defaultTimeout: number;
  maxRetries: number;
  maxConcurrency: number;
}

export interface ValidationResult {
  valid: boolean;
  errors: Array<{
    field: string;
    message: string;
  }>;
}

export interface ConfigValidationRequest {
  type: 'server' | 'llm' | 'agentOS' | 'runtime';
  config: ServerConfig | LLMConfig | AgentOSConfig | RuntimeConfig;
}

export const DEFAULT_SERVER_CONFIG: ServerConfig = {
  apiBaseUrl: 'http://localhost:8080',
  wsBaseUrl: 'ws://localhost:8080',
  temporalHost: '',
};

export const DEFAULT_LLM_CONFIG: LLMConfig = {
  apiKey: '',
  baseUrl: 'https://api.deepseek.com',
  model: 'deepseek-chat',
  temperature: 0.7,
  maxTokens: 4096,
};

export const DEFAULT_AGENT_OS_CONFIG: AgentOSConfig = {
  grpcAddress: 'localhost:50051',
  grpcTimeout: 30,
};

export const DEFAULT_RUNTIME_CONFIG: RuntimeConfig = {
  defaultTimeout: 3600,
  maxRetries: 3,
  maxConcurrency: 5,
};