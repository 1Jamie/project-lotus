import { EventEmitter } from 'events';

export enum Anchor {
  None = 0,
  Fill = 1,
  Left = 2,
  Right = 3,
  Top = 4,
  Bottom = 5
}

export type LayoutLengthJson =
  | { kind: 'fixed'; value: number }
  | { kind: 'weight'; value: number };

export interface LayoutLeafNode {
  type: 'leaf';
  paneId: string;
}

export interface LayoutSplitNode {
  type: 'split';
  id: string;
  direction: 'horizontal' | 'vertical';
  children: LayoutSplitChild[];
}

export interface LayoutSplitChild {
  node: LayoutNodeJson;
  size: LayoutLengthJson;
  min: number;
  max: number | null;
  resizable: boolean;
  visible: boolean;
  detached: boolean;
}

export interface LayoutDockChild {
  anchor: Anchor;
  node: LayoutNodeJson;
  size: LayoutLengthJson;
  min: number;
  max: number | null;
  resizable: boolean;
  visible: boolean;
  detached: boolean;
}

export interface LayoutDockRootNode {
  type: 'dockRoot';
  children: LayoutDockChild[];
}

export type LayoutNodeJson =
  | LayoutLeafNode
  | LayoutSplitNode
  | LayoutDockRootNode;

export interface LayoutScope {
  pane(id: string, sizeOrOpts?: number | Partial<PaneOptions & SplitChildOptions>, maybeOpts?: Partial<PaneOptions & SplitChildOptions>): this;
  row(id: string, fn: (scope: LayoutScope) => void): this;
  row(id: string, opts: SplitChildOptions, fn: (scope: LayoutScope) => void): this;
  col(id: string, fn: (scope: LayoutScope) => void): this;
  col(id: string, opts: SplitChildOptions, fn: (scope: LayoutScope) => void): this;
  split(id: string, opts: { direction?: 'horizontal' | 'vertical' } & SplitChildOptions, fn: (scope: LayoutScope) => void): this;
}

export interface SplitChildOptions {
  size?: number;
  weight?: number;
  width?: number;
  height?: number;
  min?: number;
  max?: number;
  /** Enables native gutter drag on Weight↔Weight (either side) or Fixed seams (Fixed side). */
  resizable?: boolean;
  visible?: boolean;
  detached?: boolean;
}

export declare class LayoutBuilder {
  constructor();
  left(id: string, width: number, options?: Partial<PaneOptions>): this;
  right(id: string, width: number, options?: Partial<PaneOptions>): this;
  top(id: string, height: number, options?: Partial<PaneOptions>): this;
  bottom(id: string, height: number, options?: Partial<PaneOptions>): this;
  fill(id: string, options?: Partial<PaneOptions>): this;
  absolute(id: string, x: number, y: number, width: number, height: number, options?: Partial<PaneOptions>): this;
  row(id: string, fn: (scope: LayoutScope) => void): this;
  row(id: string, opts: SplitChildOptions, fn: (scope: LayoutScope) => void): this;
  col(id: string, fn: (scope: LayoutScope) => void): this;
  col(id: string, opts: SplitChildOptions, fn: (scope: LayoutScope) => void): this;
  split(id: string, opts: { direction?: 'horizontal' | 'vertical' } & SplitChildOptions, fn: (scope: LayoutScope) => void): this;
  config(): { panes: PaneOptions[]; layout: LayoutDockRootNode };
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
  /**
   * Run the pane inside a process-isolated, OS-sandboxed engine
   * (Landlock FS jailing, CLONE_NEWNET network namespace, Seccomp BPF syscall lockdown,
   * zero host /tmp access, and anti-SSRF NetworkBroker HTTP proxy).
   */
  sandbox?: boolean;
  /** Deprecated alias for `sandbox` */
  secure?: boolean;
  /**
   * When `sandbox: true` is set, setting `isolated: true` spawns a dedicated, independent
   * `lotus-supervisor` + `lotus-engine` process pair and OS sandbox exclusively for this pane
   * instead of sharing the default sandboxed engine pool.
   */
  isolated?: boolean;
  /** Track 2 authorized capability channels (e.g. ['vault', 'telemetry']) */
  allowedChannels?: string[];
  /** Allowed origins for Track 2 IPC */
  trustedOrigins?: string[];
  /** Run in a separate process (defaults to true when sandbox is enabled) */
  separateProcess?: boolean;
}

export interface SelectMenuTheme {
  base?: 'light' | 'dark';
  bg?: string;
  border?: string;
  text?: string;
  textMuted?: string;
  hover?: string;
  active?: string;
  activeText?: string;
  groupLabel?: string;
  fontFamily?: string;
  fontSize?: string | number;
  borderRadius?: string | number;
  boxShadow?: string;
  maxHeight?: string | number;
}

export interface SelectMenuOptions {
  url?: string;
  css?: string;
  theme?: 'auto' | 'light' | 'dark' | SelectMenuTheme;
  enabled?: boolean;
}

export interface SelectMenuItemOption {
  type: 'option';
  id: number;
  label: string;
  is_disabled: boolean;
}

export interface SelectMenuItemOptgroup {
  type: 'optgroup';
  label: string;
  options: Array<{ id: number; label: string; is_disabled: boolean }>;
}

export type SelectMenuItem = SelectMenuItemOption | SelectMenuItemOptgroup;

export interface SelectMenuEvent {
  preventDefault(): void;
  readonly defaultPrevented: boolean;
  readonly id: string;
  readonly paneId: string;
  readonly rect: { x: number; y: number; width: number; height: number };
  readonly options: SelectMenuItem[];
  readonly selectedOptions: number[];
  readonly allowMultiple: boolean;
  submit(selectedIndices: number[]): void;
  update(selectedIndices: number[]): void;
  dismiss(): void;
}

export interface ScrollbarOptions {
  enabled?: boolean;
  theme?: 'auto' | 'light' | 'dark';
  autoHide?: boolean;
  width?: number;
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
  /** Virtual layout tree from LayoutBuilder.config().layout */
  layout?: LayoutDockRootNode;
  id?: string;
  wmClass?: string;
  /** Alias for !frameless */
  frame?: boolean;
  selectMenu?: SelectMenuOptions;
  scrollbars?: boolean | ScrollbarOptions;
  /** Run panes inside a process-isolated, OS-sandboxed engine */
  sandbox?: boolean;
  /** Deprecated alias for `sandbox` */
  secure?: boolean;
  /** Spawn a dedicated independent sandboxed engine process per pane instead of pooling */
  isolated?: boolean;
  /** Track 2 authorized capability channels */
  allowedChannels?: string[];
  trustedOrigins?: string[];
}

export interface ResizePayload {
  width: number;
  height: number;
  logicalWidth: number;
  logicalHeight: number;
  maximized?: boolean;
}

export declare class Pane extends EventEmitter {
  id: string;
  window: ServoWindow;
  visible: boolean;
  
  loadUrl(url: string): void;
  executeScript(script: string): void;
  /**
   * Reposition/resize a floating pane. Permanently sets Anchor.None
   * (detaches the pane from automatic window-resize reflow).
   */
  setRect(x: number, y: number, width: number, height: number): void;
  /**
   * Update dock thickness without clearing the pane's anchor.
   * Left/Right consume width; Top/Bottom consume height; Fill/None are no-ops.
   * A single scalar is enough for the consumed axis: `pane.setSize(240)`.
   */
  setSize(width: number, height?: number): void;
  /** Update Weight sizing for a leaf in the layout tree (tree-backed layouts). */
  setWeight(weight: number): void;
  setVisible(visible: boolean): void;
  isVisible(): boolean;
  toggleVisible(): boolean;
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
  maximized: boolean;
  panes: Map<string, Pane>;
  
  constructor(options?: string | WindowOptions);

  loadUrl(url: string): void;
  executeScript(script: string): void;
  createPane(id: string, options?: Partial<PaneOptions>): Pane;
  addPane(id: string, options?: Partial<PaneOptions>): Pane;
  getPane(id: string): Pane | undefined;
  removePane(id: string): void;

  /** Coalesce layout tree mutations; single native reflow when the callback returns. */
  batchLayout(fn: () => void): void;
  setSplitWeights(splitId: string, weights: Record<string, number> | number[]): void;
  setSplitSizes(splitId: string, sizes: Record<string, number> | number[]): void;

  setSelectMenuTheme(theme: 'auto' | 'light' | 'dark' | SelectMenuTheme): void;
  setSelectMenuCss(css: string): void;

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
  isMaximized(): boolean;
  toggleMaximize(): boolean;
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
   * - 'close' / 'closed': Window destroyed
   * - 'load-status': (status: string, paneId: string)
   * - 'title-changed': (title: string, paneId: string)
   * - 'file-drop': (data: { path: string })
   */
  on(event: 'ready-to-show', listener: () => void): this;
  on(event: 'ready' | 'dom-ready', listener: (data: any) => void): this;
  on(event: 'resize' | 'resized', listener: (payload: ResizePayload) => void): this;
  on(event: 'moved', listener: (pos: { x: number, y: number }) => void): this;
  on(event: 'focus' | 'blur' | 'close' | 'closed' | 'ready-to-show', listener: () => void): this;
  on(event: 'load-status', listener: (status: string, paneId: string) => void): this;
  on(event: 'title-changed', listener: (title: string, paneId: string) => void): this;
  on(event: 'select-menu', listener: (e: SelectMenuEvent) => void): this;
  on(event: 'file-drop' | 'file-hover', listener: (data: { path: string }) => void): this;
  on(event: string | symbol, listener: (...args: any[]) => void): this;
}

export interface SecureMethodOptions {
  channel: string;
  /** Declarative output validation and projection contract (`schema` is also supported as an alias) */
  returns?: Record<string, any>;
  schema?: Record<string, any>;
  [key: string]: any;
}

export interface SecureEventOptions {
  /** Declarative output validation and projection contract (`schema` is also supported as an alias) */
  returns?: Record<string, any>;
  schema?: Record<string, any>;
  [key: string]: any;
}

export interface IpcMain extends EventEmitter {
  send(channel: string, data: any): void;
  sendTo(clientId: string, channel: string, data: any): void;
  handle(channel: string, handler: (data: any) => any | Promise<any>): void;
  register(method: string, handler: (data: any) => any | Promise<any>, options: SecureMethodOptions): () => void;
  registerEvent(eventName: string, options: SecureEventOptions): () => void;
  unregister(method: string): boolean;
}

export const ipcMain: IpcMain;

export const app: {
  quit(): void;
  exit(code?: number): void;
  warmup(): void;
  getSystemTheme(): 'light' | 'dark';
  initVfs(options?: { required?: boolean }): boolean;
  isVfsInitialized(): boolean;
  validatePlatformSecurityIsolation(options?: { sandbox?: boolean; secure?: boolean; separateProcess?: boolean }): void;
  detectLinuxThemeNode(): 'light' | 'dark' | null;
  spawnSandboxedEngine(options?: any): Promise<any>;
};

export function validatePlatformSecurityIsolation(options?: { sandbox?: boolean; secure?: boolean; separateProcess?: boolean }): void;
export function detectLinuxThemeNode(): 'light' | 'dark' | null;
export function spawnSandboxedEngine(options?: any): Promise<any>;
export const launcher: any;
export const networkBroker: any;
export const secureConstellation: any;
export const originDerivation: any;
export const hostIpcPolicy: any;
export const hostIpcRouter: any;
