import { useEffect, useRef } from 'react';
import * as PIXI from 'pixi.js';
import { Live2DModel } from 'pixi-live2d-display';

// 关键：设置全局 PIXI（pixi-live2d-display 需要）
(window as any).PIXI = PIXI;

interface Live2DCanvasProps {
  modelPath: string;
  width: number;
  height: number;
  onModelLoaded?: (model: any) => void;
  onError?: (error: Error) => void;
}

export function Live2DCanvas({ modelPath, width, height, onModelLoaded, onError }: Live2DCanvasProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const appRef = useRef<PIXI.Application | null>(null);
  const modelRef = useRef<any>(null);
  const initializedRef = useRef(false);

  console.log('[Live2DCanvas] Component rendered, modelPath:', modelPath);

  useEffect(() => {
    if (!canvasRef.current || initializedRef.current) {
      console.warn('[Live2DCanvas] Already initialized or canvas ref is null');
      return;
    }

    initializedRef.current = true;
    console.log('[Live2DCanvas] Initializing...');

    try {
      // 创建 PIXI 应用
      const app = new PIXI.Application({
        view: canvasRef.current,
        width,
        height,
        backgroundAlpha: 0,
      });
      appRef.current = app;

      console.log('[Live2DCanvas] PIXI app created, loading model from:', modelPath);

      // 加载模型
      Live2DModel.from(modelPath)
        .then((model) => {
          console.log('[Live2DCanvas] Model loaded!', model.width, model.height);

          // 缩放模型以适应窗口，底部对齐
          const scale = Math.min(width / model.width, height / model.height) * 1;
          console.log('[Live2DCanvas] Scale:', scale);
          model.scale.set(scale);
          model.anchor.set(0.5, 1.0);
          model.x = width / 2;
          model.y = height;

          console.log('[Live2DCanvas] Model position:', model.x, model.y);

          // 添加到舞台
          app.stage.addChild(model);
          modelRef.current = model;

          // 添加点击交互
          model.on('hit', (hitAreas: string[]) => {
            console.log('[Live2DCanvas] Hit:', hitAreas);
            if (hitAreas.includes('Head')) {
              model.expression();
            }
          });

          onModelLoaded?.(model);
        })
        .catch((error) => {
          console.error('[Live2DCanvas] Error:', error);
          onError?.(error);
        });
    } catch (error) {
      console.error('[Live2DCanvas] Failed to create PIXI app:', error);
      onError?.(error as Error);
    }

    // 不在清理函数中销毁 PIXI 应用
    return () => {
      // 只清理引用，不销毁应用
      modelRef.current = null;
    };
  }, [modelPath, width, height]);

  // 鼠标眼神跟随
  useEffect(() => {
    const handleMouseMove = (e: MouseEvent) => {
      if (!modelRef.current) return;

      // 将鼠标坐标转换为 [-1, 1] 范围
      // 以窗口中心为原点
      const x = (e.clientX / window.innerWidth) * 2 - 1;
      const y = (e.clientY / window.innerHeight) * 2 - 1;

      // 调用内置的 focus 方法，让模型看向鼠标位置
      // focus 方法会自动处理眼球和头部的平滑跟随
      modelRef.current.focus(x, y);
    };

    const handleMouseLeave = () => {
      // 鼠标离开窗口时，让模型回到中心位置
      if (modelRef.current) {
        modelRef.current.focus(0, 0);
      }
    };

    // 监听全局鼠标移动
    window.addEventListener('mousemove', handleMouseMove);
    window.addEventListener('mouseleave', handleMouseLeave);

    return () => {
      window.removeEventListener('mousemove', handleMouseMove);
      window.removeEventListener('mouseleave', handleMouseLeave);
    };
  }, []);

  return (
    <canvas
      ref={canvasRef}
      style={{
        width,
        height,
        display: 'block',
        pointerEvents: 'none', // 允许鼠标事件穿透到下层
      }}
    />
  );
}
