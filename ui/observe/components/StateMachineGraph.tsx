"use client";

import { useMemo, useState, useRef, useCallback } from "react";
import dagre from "@dagrejs/dagre";
import type { SpecDetail } from "@/lib/types";

interface StateMachineGraphProps {
  spec: SpecDetail;
}

interface LayoutNode {
  name: string;
  x: number;
  y: number;
  isInitial: boolean;
  isTerminal: boolean;
}

interface LayoutEdge {
  from: string;
  to: string;
  label: string;
  isSelfLoop: boolean;
  points: Array<{ x: number; y: number }>;
}

const NODE_WIDTH = 140;
const NODE_HEIGHT = 50;
const PADDING = 60;

export default function StateMachineGraph({ spec }: StateMachineGraphProps) {
  const [zoom, setZoom] = useState(1);
  const [pan, setPan] = useState({ x: 0, y: 0 });
  const [dragging, setDragging] = useState(false);
  const dragStart = useRef({ x: 0, y: 0, panX: 0, panY: 0 });
  const containerRef = useRef<HTMLDivElement>(null);

  const { nodes, edges, width, height } = useMemo(() => {
    const terminalStates = new Set<string>();
    for (const inv of spec.invariants) {
      if (inv.name === "no_further_transitions" || inv.assertion.includes("no outgoing")) {
        for (const state of inv.when) {
          terminalStates.add(state);
        }
      }
    }

    const g = new dagre.graphlib.Graph();
    g.setGraph({
      rankdir: "TB",
      ranksep: 80,
      nodesep: 60,
      marginx: PADDING,
      marginy: PADDING,
    });
    g.setDefaultEdgeLabel(() => ({}));

    for (const state of spec.states) {
      g.setNode(state, { width: NODE_WIDTH, height: NODE_HEIGHT });
    }

    // Merge edges with same from→to
    const edgeMap = new Map<string, string[]>();
    for (const action of spec.actions) {
      const to = action.to ?? action.from[0];
      for (const from of action.from) {
        const key = `${from}->${to}`;
        if (!edgeMap.has(key)) edgeMap.set(key, []);
        const labels = edgeMap.get(key)!;
        if (!labels.includes(action.name)) labels.push(action.name);
      }
    }

    for (const [key] of edgeMap) {
      const [from, to] = key.split("->");
      if (from !== to) {
        g.setEdge(from, to);
      }
    }

    dagre.layout(g);

    const layoutNodes: LayoutNode[] = spec.states.map((name) => {
      const node = g.node(name);
      return {
        name,
        x: node.x,
        y: node.y,
        isInitial: name === spec.initial_state,
        isTerminal: terminalStates.has(name),
      };
    });

    const layoutEdges: LayoutEdge[] = [];
    for (const [key, labels] of edgeMap) {
      const [from, to] = key.split("->");
      const isSelfLoop = from === to;
      let points: Array<{ x: number; y: number }> = [];

      if (!isSelfLoop) {
        const edge = g.edge(from, to);
        if (edge?.points) {
          points = edge.points;
        }
      }

      layoutEdges.push({
        from,
        to,
        label: labels.join(", "),
        isSelfLoop,
        points,
      });
    }

    const graphLabel = g.graph();
    const svgWidth = (graphLabel.width ?? 600) + PADDING;
    const svgHeight = (graphLabel.height ?? 400) + PADDING;

    return {
      nodes: layoutNodes,
      edges: layoutEdges,
      width: svgWidth,
      height: svgHeight,
    };
  }, [spec]);

  const getNode = (name: string) => nodes.find((n) => n.name === name);

  const handleWheel = useCallback((e: React.WheelEvent) => {
    e.preventDefault();
    const delta = e.deltaY > 0 ? 0.9 : 1.1;
    setZoom((z) => Math.min(3, Math.max(0.3, z * delta)));
  }, []);

  const handleMouseDown = useCallback(
    (e: React.MouseEvent) => {
      if (e.button !== 0) return;
      setDragging(true);
      dragStart.current = { x: e.clientX, y: e.clientY, panX: pan.x, panY: pan.y };
    },
    [pan],
  );

  const handleMouseMove = useCallback(
    (e: React.MouseEvent) => {
      if (!dragging) return;
      const dx = e.clientX - dragStart.current.x;
      const dy = e.clientY - dragStart.current.y;
      setPan({ x: dragStart.current.panX + dx, y: dragStart.current.panY + dy });
    },
    [dragging],
  );

  const handleMouseUp = useCallback(() => {
    setDragging(false);
  }, []);

  const resetView = useCallback(() => {
    setZoom(1);
    setPan({ x: 0, y: 0 });
  }, []);

  /** Build a smooth path from dagre edge points, clipping to node boundaries. */
  const buildEdgePath = (edge: LayoutEdge): string | null => {
    const fromNode = getNode(edge.from);
    const toNode = getNode(edge.to);
    if (!fromNode || !toNode || edge.points.length < 2) return null;

    // Clip start point to bottom edge of source node
    const startX = fromNode.x;
    const startY = fromNode.y + NODE_HEIGHT / 2;
    // Clip end point to top edge of target node
    const endX = toNode.x;
    const endY = toNode.y - NODE_HEIGHT / 2;

    if (edge.points.length === 2) {
      return `M ${startX} ${startY} L ${endX} ${endY}`;
    }

    // Use dagre intermediate points as control/via points
    const mid = edge.points.slice(1, -1);
    let d = `M ${startX} ${startY}`;
    for (const pt of mid) {
      d += ` L ${pt.x} ${pt.y}`;
    }
    d += ` L ${endX} ${endY}`;
    return d;
  };

  return (
    <div className="bg-[#111115] rounded-lg p-4 relative">
      {/* Controls */}
      <div className="absolute top-3 right-3 z-10 flex items-center gap-1.5">
        <span className="text-[10px] text-zinc-600 font-mono mr-1">
          {Math.round(zoom * 100)}%
        </span>
        <button
          onClick={() => setZoom((z) => Math.min(3, z * 1.2))}
          className="w-6 h-6 flex items-center justify-center rounded bg-white/[0.04] hover:bg-white/[0.08] text-zinc-400 text-xs transition-colors"
          title="Zoom in"
        >
          +
        </button>
        <button
          onClick={() => setZoom((z) => Math.max(0.3, z * 0.8))}
          className="w-6 h-6 flex items-center justify-center rounded bg-white/[0.04] hover:bg-white/[0.08] text-zinc-400 text-xs transition-colors"
          title="Zoom out"
        >
          -
        </button>
        <button
          onClick={resetView}
          className="px-2 h-6 flex items-center justify-center rounded bg-white/[0.04] hover:bg-white/[0.08] text-zinc-500 text-[10px] transition-colors"
          title="Fit to view"
        >
          Fit
        </button>
      </div>

      <div
        ref={containerRef}
        className="overflow-hidden"
        style={{ cursor: dragging ? "grabbing" : "grab" }}
        onWheel={handleWheel}
        onMouseDown={handleMouseDown}
        onMouseMove={handleMouseMove}
        onMouseUp={handleMouseUp}
        onMouseLeave={handleMouseUp}
      >
        <svg
          width={width}
          height={height}
          viewBox={`0 0 ${width} ${height}`}
          className="mx-auto"
          style={{
            transform: `translate(${pan.x}px, ${pan.y}px) scale(${zoom})`,
            transformOrigin: "center center",
          }}
        >
          <defs>
            <marker
              id="arrowhead"
              markerWidth="10"
              markerHeight="7"
              refX="10"
              refY="3.5"
              orient="auto"
            >
              <polygon points="0 0, 10 3.5, 0 7" fill="#52525b" />
            </marker>
            <marker
              id="arrowhead-teal"
              markerWidth="10"
              markerHeight="7"
              refX="10"
              refY="3.5"
              orient="auto"
            >
              <polygon points="0 0, 10 3.5, 0 7" fill="#2dd4bf" />
            </marker>
          </defs>

          {/* Draw edges */}
          {edges.map((edge, i) => {
            const fromNode = getNode(edge.from);
            const toNode = getNode(edge.to);
            if (!fromNode || !toNode) return null;

            if (edge.isSelfLoop) {
              const cx = fromNode.x;
              const cy = fromNode.y - NODE_HEIGHT / 2;
              return (
                <g key={i}>
                  <path
                    d={`M ${cx - 20} ${cy} C ${cx - 30} ${cy - 45}, ${cx + 30} ${cy - 45}, ${cx + 20} ${cy}`}
                    fill="none"
                    stroke="#3f3f46"
                    strokeWidth="1.5"
                    markerEnd="url(#arrowhead)"
                  />
                  <text
                    x={cx}
                    y={cy - 35}
                    textAnchor="middle"
                    className="text-[10px] fill-zinc-400 font-mono"
                  >
                    {edge.label}
                  </text>
                </g>
              );
            }

            const path = buildEdgePath(edge);
            if (!path) return null;

            // Check for reverse edge to offset label
            const hasReverse = edges.some(
              (e) => e.from === edge.to && e.to === edge.from && !e.isSelfLoop,
            );

            // Label position: midpoint of path
            const midIdx = Math.floor(edge.points.length / 2);
            const labelPt = edge.points[midIdx] ?? {
              x: (fromNode.x + toNode.x) / 2,
              y: (fromNode.y + toNode.y) / 2,
            };
            const labelOffset = hasReverse ? -14 : -8;

            return (
              <g key={i}>
                <path
                  d={path}
                  fill="none"
                  stroke="#3f3f46"
                  strokeWidth="1.5"
                  markerEnd="url(#arrowhead)"
                />
                <text
                  x={labelPt.x}
                  y={labelPt.y + labelOffset}
                  textAnchor="middle"
                  className="text-[10px] fill-zinc-400 font-mono"
                >
                  {edge.label}
                </text>
              </g>
            );
          })}

          {/* Draw initial state arrow */}
          {nodes
            .filter((n) => n.isInitial)
            .map((node) => (
              <g key={`init-${node.name}`}>
                <line
                  x1={node.x - 100}
                  y1={node.y}
                  x2={node.x - NODE_WIDTH / 2 - 2}
                  y2={node.y}
                  stroke="#2dd4bf"
                  strokeWidth="2"
                  markerEnd="url(#arrowhead-teal)"
                />
                <circle
                  cx={node.x - 105}
                  cy={node.y}
                  r="4"
                  fill="#2dd4bf"
                />
              </g>
            ))}

          {/* Draw state nodes */}
          {nodes.map((node) => (
            <g key={node.name}>
              {node.isTerminal && (
                <rect
                  x={node.x - NODE_WIDTH / 2 - 5}
                  y={node.y - NODE_HEIGHT / 2 - 5}
                  width={NODE_WIDTH + 10}
                  height={NODE_HEIGHT + 10}
                  rx={10}
                  fill="none"
                  stroke="#6b7280"
                  strokeWidth="1.5"
                  strokeDasharray="4 2"
                />
              )}
              <rect
                x={node.x - NODE_WIDTH / 2 + 2}
                y={node.y - NODE_HEIGHT / 2 + 2}
                width={NODE_WIDTH - 4}
                height={NODE_HEIGHT - 4}
                rx={8}
                fill={node.isInitial ? "#042f2e" : node.isTerminal ? "#1c1917" : "#111115"}
                stroke={node.isInitial ? "#2dd4bf" : node.isTerminal ? "#78716c" : "#3f3f46"}
                strokeWidth={node.isInitial ? 2 : 1.5}
              />
              <text
                x={node.x}
                y={node.y + 1}
                textAnchor="middle"
                dominantBaseline="middle"
                className={`text-sm font-mono ${
                  node.isInitial ? "fill-teal-400" : node.isTerminal ? "fill-zinc-500" : "fill-zinc-200"
                }`}
              >
                {node.name}
              </text>
            </g>
          ))}
        </svg>
      </div>
    </div>
  );
}
