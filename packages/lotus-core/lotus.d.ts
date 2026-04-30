import { EventEmitter } from 'events';

export enum Anchor {
  None = 0,
  Fill = 1,
  Left = 2,
  Right = 3,
  Top = 4,
  Bottom = 5
}

export declare class LayoutBuilder {
  constructor();
  left(id: string, width: number, options?: Partial<PaneOptions>): this;
  right(id: string, width: number, options?: Partial<PaneOptions>): this;
  top(id: string, height: number, options?: Partial<PaneOptions>): this;
  bottom(id: string, height: number, options?: Partial<PaneOptions>): this;
  fill(id: string, options?: Partial<PaneOptions>): this;
  absolute(id: string, x: number, y: number, width: number, height: number, options?: Partial<PaneOptions>): this;
  config(): { panes: PaneOptions[] };
}

export interface PaneOptions {
  id: string;
  url: string;
  x: number;
  y: number;
  width: number;
  height: number;
  zIndex: number;
  visible: boolean;
  anchor?: Anchor;
  dockOrder?: number;
}

export interface WindowOptions {
  width?: number;
  height?: number;
  maximized?: boolean;
  fullscreen?: boolean;
  title?: string;
  resizable?: boolean;
  frameless?: boolean;
  alwaysOnTop?: boolean;
  initialUrl?: string;
  restoreState?: boolean;
  root?: string;
  index?: string;
  transparent?: boolean;
  cornerRadius?: number;
  visible?: boolean;
  autoResizeMain?: boolean;
  panes?: Array<Partial<PaneOptions>>;
  id?: string;
  wmClass?: string;
  /** Alias for !frameless */
  frame?: boolean;
}

export interface ResizePayload {
  width: number;
  height: number;
  logicalWidth: number;
  logicalHeight: number;
}

export declare class Pane extends EventEmitter {
  id: string;
  window: ServoWindow;
  
  loadUrl(url: string): void;
  executeScript(script: string): void;
  setRect(x: number, y: number, width: number, height: number): void;
  setVisible(visible: boolean): void;
  focus(): void;
  updateDragRegions(rects: any[]): void;
  remove(): void;

  /**
   * Events:
   * - 'load-status': (status: 'started' | 'head-parsed' | 'complete')
   * - 'title-changed': (title: string)
   */
  on(event: 'load-status', listener: (status: 'started' | 'head-parsed' | 'complete') => void): this;
  on(event: 'title-changed', listener: (title: string) => void): this;
  on(event: string | symbol, listener: (...args: any[]) => void): this;
}

export declare class ServoWindow extends EventEmitter {
  id: string;
  panes: Map<string, Pane>;
  
  constructor(options?: string | WindowOptions);

  loadUrl(url: string): void;
  executeScript(script: string): void;
  createPane(id: string, options?: Partial<PaneOptions>): Pane;
  getPane(id: string): Pane | undefined;
  removePane(id: string): void;

  show(): void;
  hide(): void;
  close(): void;
  setTitle(title: string): void;
  setSize(width: number, height: number): void;
  setMinSize(width: number, height: number): void;
  setMaxSize(width: number, height: number): void;
  setPosition(x: number, y: number): void;
  maximize(): void;
  unmaximize(): void;
  minimize(): void;
  unminimize(): void;
  focus(): void;

  sendToRenderer(channel: string, data: any, immediate?: boolean): void;
  sendToPaneRenderer(paneId: string, channel: string, data: any, immediate?: boolean): void;

  /**
   * Events:
   * - 'ready-to-show': First frame rendered
   * - 'ready': IPC and DOM ready
   * - 'dom-ready': Alias for 'ready'
   * - 'resize' / 'resized': (payload: ResizePayload)
   * - 'moved': (pos: { x: number, y: number })
   * - 'focus' / 'blur': Window focus state
   * - 'closed': Window destroyed
   * - 'load-status': (status: string, paneId: string)
   * - 'title-changed': (title: string, paneId: string)
   * - 'file-drop': (data: { path: string })
   */
  on(event: 'ready-to-show', listener: () => void): this;
  on(event: 'ready' | 'dom-ready', listener: (data: any) => void): this;
  on(event: 'resize' | 'resized', listener: (payload: ResizePayload) => void): this;
  on(event: 'moved', listener: (pos: { x: number, y: number }) => void): this;
  on(event: 'focus' | 'blur' | 'closed' | 'ready-to-show', listener: () => void): this;
  on(event: 'load-status', listener: (status: string, paneId: string) => void): this;
  on(event: 'title-changed', listener: (title: string, paneId: string) => void): this;
  on(event: 'file-drop' | 'file-hover', listener: (data: { path: string }) => void): this;
  on(event: string | symbol, listener: (...args: any[]) => void): this;
}

export interface IpcMain extends EventEmitter {
  send(channel: string, data: any): void;
  sendTo(windowId: string, channel: string, data: any): void;
  handle(channel: string, handler: (data: any) => any | Promise<any>): void;
}

export const ipcMain: IpcMain;

export const app: {
  quit(): void;
  warmup(): void;
  initVfs(): void;
};
