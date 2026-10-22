import React from 'react';
import { Card, Tabs, Form, Input, Button, InputNumber, Slider, Space, App } from 'antd';
import { SaveOutlined, ReloadOutlined } from '@ant-design/icons';
import { useSettingsStore } from '@/stores';

const Settings: React.FC = () => {
  const { server, llm, agentOS, runtime, updateServerConfig, updateLLMConfig, updateAgentOSConfig, updateRuntimeConfig, saveSettings, resetToDefaults, loadSettings } = useSettingsStore();
  const [activeTab, setActiveTab] = React.useState('server');
  const { message } = App.useApp();

  React.useEffect(() => { loadSettings(); }, [loadSettings]);

  const handleSave = async () => {
    try {
      await saveSettings();
      message.success('配置已保存并同步到后端');
    } catch {
      message.error('配置保存失败');
    }
  };

  const tabItems = [
    {
      key: 'server', label: '服务端配置',
      children: (
        <Form layout="vertical">
          <Form.Item label="API 地址">
            <Input value={server.apiBaseUrl} onChange={(e) => updateServerConfig({ apiBaseUrl: e.target.value })} placeholder="http://localhost:8080" />
          </Form.Item>
          <Form.Item label="WebSocket 地址">
            <Input value={server.wsBaseUrl} onChange={(e) => updateServerConfig({ wsBaseUrl: e.target.value })} placeholder="ws://localhost:8080" />
          </Form.Item>
          <Form.Item label="Temporal Server 地址">
            <Input value={server.temporalHost} onChange={(e) => updateServerConfig({ temporalHost: e.target.value })} placeholder="temporal.example.com:7233" />
          </Form.Item>
        </Form>
      ),
    },
    {
      key: 'llm', label: 'LLM 配置',
      children: (
        <Form layout="vertical">
          <Form.Item label="API Key">
            <Input.Password value={llm.apiKey} onChange={(e) => updateLLMConfig({ apiKey: e.target.value })} placeholder="sk-xxx" />
          </Form.Item>
          <Form.Item label="Base URL">
            <Input value={llm.baseUrl} onChange={(e) => updateLLMConfig({ baseUrl: e.target.value })} placeholder="https://api.deepseek.com" />
          </Form.Item>
          <Form.Item label="模型">
            <Input value={llm.model} onChange={(e) => updateLLMConfig({ model: e.target.value })} placeholder="deepseek-chat" />
          </Form.Item>
          <Form.Item label={`Temperature: ${llm.temperature}`}>
            <Slider min={0} max={2} step={0.1} value={llm.temperature} onChange={(value) => updateLLMConfig({ temperature: value })} />
          </Form.Item>
          <Form.Item label="最大 Tokens">
            <InputNumber value={llm.maxTokens} onChange={(value) => updateLLMConfig({ maxTokens: value || 4096 })} min={1} max={32000} />
          </Form.Item>
        </Form>
      ),
    },
    {
      key: 'agentOS', label: 'Agent OS 配置',
      children: (
        <Form layout="vertical">
          <Form.Item label="gRPC 地址">
            <Input value={agentOS.grpcAddress} onChange={(e) => updateAgentOSConfig({ grpcAddress: e.target.value })} placeholder="localhost:50051" />
          </Form.Item>
          <Form.Item label="超时时间（秒）">
            <InputNumber value={agentOS.grpcTimeout} onChange={(value) => updateAgentOSConfig({ grpcTimeout: value || 30 })} min={1} max={300} />
          </Form.Item>
        </Form>
      ),
    },
    {
      key: 'runtime', label: '运行参数',
      children: (
        <Form layout="vertical">
          <Form.Item label="默认超时（秒）">
            <InputNumber value={runtime.defaultTimeout} onChange={(value) => updateRuntimeConfig({ defaultTimeout: value || 3600 })} min={60} max={86400} />
          </Form.Item>
          <Form.Item label="最大重试次数">
            <InputNumber value={runtime.maxRetries} onChange={(value) => updateRuntimeConfig({ maxRetries: value || 3 })} min={0} max={10} />
          </Form.Item>
          <Form.Item label="最大并发数">
            <InputNumber value={runtime.maxConcurrency} onChange={(value) => updateRuntimeConfig({ maxConcurrency: value || 5 })} min={1} max={20} />
          </Form.Item>
        </Form>
      ),
    },
  ];

  return (
    <div>
      <Card
        title="系统设置"
        extra={
          <Space>
            <Button icon={<ReloadOutlined />} onClick={resetToDefaults}>恢复默认</Button>
            <Button type="primary" icon={<SaveOutlined />} onClick={handleSave}>保存</Button>
          </Space>
        }
      >
        <Tabs activeKey={activeTab} onChange={setActiveTab} items={tabItems} />
      </Card>
    </div>
  );
};

export default Settings;