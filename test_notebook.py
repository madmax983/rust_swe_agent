import json

def generate_notebook():
    notebook = {
        "cells": [
            {
                "cell_type": "markdown",
                "metadata": {},
                "source": [
                    "# Trajectory Export\n"
                ]
            }
        ],
        "metadata": {},
        "nbformat": 4,
        "nbformat_minor": 5
    }
    return json.dumps(notebook)

print(generate_notebook())
