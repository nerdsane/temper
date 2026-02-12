"use client";

import { useMemo } from "react";
import type { SpecDetail } from "@/lib/types";

interface StateMachineGraphProps {
  spec: SpecDetail;
}

interface StateNode {
  name: string;
  x: number;
  y: number;
  isInitial: boolean;
  isTerminal: boolean;
}

interface TransitionEdge {
  from: string;
  to: string;
  label: string;
  isSelfLoop: boolean;
}

export default function StateMachineGraph({ spec }: StateMachineGraphProps) {
  const { nodes, edges, width, height } = useMemo(() => {
    const stateCount = spec.states.length;
    const terminalStates = new Set<string>();
    for (const inv of spec.invariants) {
      if (inv.name === "no_further_transitions" || inv.assertion.includes("no outgoing")) {
        for (const state of inv.when) {
          terminalStates.add(state);
        }
      }
    }

    const cols = Math.min(stateCount, 3);
    const rows = Math.ceil(stateCount / cols);
    const nodeWidth = 140;
    const nodeHeight = 50;
    const hSpacing = 220;
    const vSpacing = 120;
    const padding = 80;

    const svgWidth = cols * hSpacing + padding * 2;
    const svgHeight = rows * vSpacing + padding * 2;

    const stateNodes: StateNode[] = spec.states.map((name, i) => {
      const col = i % cols;
      const row = Math.floor(i / cols);
      return {
        name,
        x: padding + col * hSpacing + nodeWidth / 2,
        y: padding + row * vSpacing + nodeHeight / 2,
        isInitial: name === spec.initial_state,
        isTerminal: terminalStates.has(name),
      };
    });

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

    const transitionEdges: TransitionEdge[] = [];
    for (const [key, labels] of edgeMap) {
      const [from, to] = key.split("->");
      transitionEdges.push({
        from,
        to,
        label: labels.join(", "),
        isSelfLoop: from === to,
      });
    }

    return {
      nodes: stateNodes,
      edges: transitionEdges,
      width: svgWidth,
      height: svgHeight,
    };
  }, [spec]);

  const getNode = (name: string) => nodes.find((n) => n.name === name);

  return (
    <div className="bg-gray-900 border border-gray-800 rounded-lg p-4 overflow-auto">
      <svg
        width={width}
        height={height}
        viewBox={`0 0 ${width} ${height}`}
        className="mx-auto"
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
            <polygon points="0 0, 10 3.5, 0 7" fill="#6b7280" />
          </marker>
          <marker
            id="arrowhead-blue"
            markerWidth="10"
            markerHeight="7"
            refX="10"
            refY="3.5"
            orient="auto"
          >
            <polygon points="0 0, 10 3.5, 0 7" fill="#3b82f6" />
          </marker>
        </defs>

        {/* Draw edges */}
        {edges.map((edge, i) => {
          const fromNode = getNode(edge.from);
          const toNode = getNode(edge.to);
          if (!fromNode || !toNode) return null;

          if (edge.isSelfLoop) {
            const cx = fromNode.x;
            const cy = fromNode.y - 25;
            return (
              <g key={i}>
                <path
                  d={`M ${cx - 20} ${cy} C ${cx - 30} ${cy - 45}, ${cx + 30} ${cy - 45}, ${cx + 20} ${cy}`}
                  fill="none"
                  stroke="#4b5563"
                  strokeWidth="1.5"
                  markerEnd="url(#arrowhead)"
                />
                <text
                  x={cx}
                  y={cy - 35}
                  textAnchor="middle"
                  className="text-[10px] fill-gray-400 font-mono"
                >
                  {edge.label}
                </text>
              </g>
            );
          }

          const dx = toNode.x - fromNode.x;
          const dy = toNode.y - fromNode.y;
          const dist = Math.sqrt(dx * dx + dy * dy);
          const nx = dx / dist;
          const ny = dy / dist;

          const startX = fromNode.x + nx * 70;
          const startY = fromNode.y + ny * 25;
          const endX = toNode.x - nx * 70;
          const endY = toNode.y - ny * 25;

          const midX = (startX + endX) / 2;
          const midY = (startY + endY) / 2;

          const hasReverse = edges.some(
            (e) => e.from === edge.to && e.to === edge.from && !e.isSelfLoop
          );
          const perpX = -ny * (hasReverse ? 12 : 0);
          const perpY = nx * (hasReverse ? 12 : 0);

          return (
            <g key={i}>
              <line
                x1={startX + perpX}
                y1={startY + perpY}
                x2={endX + perpX}
                y2={endY + perpY}
                stroke="#4b5563"
                strokeWidth="1.5"
                markerEnd="url(#arrowhead)"
              />
              <text
                x={midX + perpX}
                y={midY + perpY - 8}
                textAnchor="middle"
                className="text-[10px] fill-gray-400 font-mono"
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
                x2={node.x - 72}
                y2={node.y}
                stroke="#22c55e"
                strokeWidth="2"
                markerEnd="url(#arrowhead-blue)"
              />
              <circle
                cx={node.x - 105}
                cy={node.y}
                r="4"
                fill="#22c55e"
              />
            </g>
          ))}

        {/* Draw state nodes */}
        {nodes.map((node) => (
          <g key={node.name}>
            {node.isTerminal && (
              <rect
                x={node.x - 73}
                y={node.y - 28}
                width={146}
                height={56}
                rx={10}
                fill="none"
                stroke="#6b7280"
                strokeWidth="1.5"
                strokeDasharray="4 2"
              />
            )}
            <rect
              x={node.x - 68}
              y={node.y - 23}
              width={136}
              height={46}
              rx={8}
              fill={node.isInitial ? "#052e16" : node.isTerminal ? "#1c1917" : "#111827"}
              stroke={node.isInitial ? "#22c55e" : node.isTerminal ? "#78716c" : "#374151"}
              strokeWidth={node.isInitial ? 2 : 1.5}
            />
            <text
              x={node.x}
              y={node.y + 1}
              textAnchor="middle"
              dominantBaseline="middle"
              className={`text-sm font-mono ${
                node.isInitial ? "fill-green-400" : node.isTerminal ? "fill-gray-500" : "fill-gray-200"
              }`}
            >
              {node.name}
            </text>
          </g>
        ))}
      </svg>
    </div>
  );
}
