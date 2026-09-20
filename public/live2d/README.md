# Live2D 模型资源目录

此目录用于存放 Live2D 模型文件。

## 目录结构

```
live2d/
└── konata/                    # 模型名称
    ├── model.model3.json      # 模型配置文件（必需）
    ├── model.moc3             # 模型文件（必需）
    ├── model.physics3.json    # 物理演算配置（可选）
    ├── pose.pose3.json        # 姿势配置（可选）
    ├── expressions/           # 表情文件目录
    │   ├── happy.exp3.json
    │   ├── sad.exp3.json
    │   └── ...
    ├── motions/               # 动作文件目录
    │   ├── idle.motion3.json
    │   ├── nod.motion3.json
    │   └── ...
    └── textures/              # 贴图文件目录
        ├── texture_00.png
        └── ...
```

## 如何添加模型

1. **获取 Live2D 模型**
   - 使用 Live2D Cubism Editor 制作模型
   - 从网上下载现成的 Cubism 4 格式模型（`.model3.json`）
   - 官方示例模型：https://github.com/Live2D/CubismWebSamples

2. **放置模型文件**
   - 将模型文件夹放入此目录（例如：`live2d/konata/`）
   - 确保 `model.model3.json` 文件存在

3. **修改代码中的模型路径**
   - 打开 `src/components/float/FloatingWidget.tsx`
   - 修改 `modelPath` 属性指向你的模型文件：
     ```tsx
     <Live2DCanvas
       modelPath="/live2d/你的模型目录/model.model3.json"
       // ...其他配置
     />
     ```

## 模型配置说明

`model.model3.json` 是模型的主配置文件，包含以下主要部分：

- **FileReferences**: 引用的所有文件
  - `Moc`: 模型文件路径
  - `Textures`: 贴图文件路径列表
  - `Physics`: 物理演算配置
  - `Expressions`: 表情配置列表
  - `Motions`: 动作配置字典

- **Groups**: 参数组配置
  - `EyeBlink`: 眨眼参数
  - `LipSync`: 口型同步参数

- **HitAreas`: 命中区域配置（用于点击交互）

## 推荐模型资源

- [Live2D 官方示例](https://github.com/Live2D/CubismWebSamples)
- [Hiyori 示例模型](https://cdn.jsdelivr.net/gh/guansss/pixi-live2d-display/test/assets/hiyori/)
- [VTuber 模型资源](https://booth.pm/)（搜索 Live2D）

## 注意事项

1. 确保模型文件格式为 Cubism 4（`.model3.json`）
2. 贴图文件建议使用 PNG 格式（支持透明通道）
3. 模型文件不要太大，建议 < 5MB，以保证性能
4. 如果模型加载失败，请检查浏览器控制台的错误信息

## 测试模型

你可以使用在线示例模型进行测试：

```tsx
<Live2DCanvas
  modelPath="https://cdn.jsdelivr.net/gh/guansss/pixi-live2d-display/test/assets/hiyori/hiyori_pro_t10.model3.json"
  // ...其他配置
/>
```

这将加载一个在线的示例模型，无需本地文件。
